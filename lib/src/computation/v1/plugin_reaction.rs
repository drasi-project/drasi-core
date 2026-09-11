// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::{plugin_services::PluginObservations, query_catalog::CatalogQuery, *};
use crate::{
    channels::{ComponentStatus, QueryResult, ResultDiff},
    context::ReactionRuntimeContext,
    queries::output_state::{
        FetchError, OutboxGap, OutboxResponse, OutboxStream, SnapshotResponse, SnapshotStream,
    },
    reactions::{
        bootstrap_context::{BootstrapBackend, BootstrapContext},
        checkpoint::ReactionCheckpoint,
        SnapshotFetcher,
    },
    recovery::ReactionRecoveryPolicy,
    Reaction,
};
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReactionPluginOptions {
    pub recovery: Option<ReactionRecoveryPolicy>,
    pub bootstrap_timeout_secs: u64,
}
impl Default for ReactionPluginOptions {
    fn default() -> Self {
        Self {
            recovery: None,
            bootstrap_timeout_secs: 300,
        }
    }
}

#[async_trait]
pub trait ReactionPluginConstructor: Send + Sync {
    async fn create(&self) -> anyhow::Result<Box<dyn Reaction>>;
}

#[derive(Clone)]
pub(super) struct OutputView {
    snapshot: QuerySnapshot,
    retained: Vec<ChangeEnvelope>,
    config_hash: u64,
    incarnation: Option<uuid::Uuid>,
}
pub(super) fn output_view(query: &CatalogQuery) -> anyhow::Result<OutputView> {
    let state = query
        .results
        .state
        .read()
        .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
    Ok(OutputView {
        snapshot: QuerySnapshot {
            rows: state.rows.clone(),
            as_of_sequence: state.sequence,
            generation: state.generation,
        },
        retained: state.outbox.iter().cloned().collect(),
        config_hash: query.config_hash,
        incarnation: *query
            .incarnation
            .read()
            .map_err(|_| anyhow::anyhow!("query output identity poisoned"))?,
    })
}
pub(super) fn snapshot_response(
    query_id: &str,
    view: &OutputView,
) -> anyhow::Result<SnapshotResponse> {
    if view.snapshot.rows.is_empty() {
        return Ok(SnapshotResponse::new(
            im::HashMap::new(),
            view.snapshot.as_of_sequence,
            view.config_hash,
        ));
    }
    let envelope = QueryChangeCodec::snapshot_envelope(
        query_id,
        &view.snapshot,
        &ComponentId::try_new("snapshot-bridge")?,
        SystemMetadata::new(
            StreamId::try_new("snapshot-bridge/out")?,
            view.snapshot.as_of_sequence,
        ),
    )?;
    let result = QueryChangeCodec::to_legacy_result(&envelope)?;
    let mut rows = im::HashMap::new();
    for row in result.results {
        match row {
            ResultDiff::Add {
                data,
                row_signature,
            } => {
                rows.insert(row_signature, data);
            }
            _ => anyhow::bail!("snapshot codec produced a non-add row"),
        }
    }
    Ok(SnapshotResponse::new(
        rows,
        view.snapshot.as_of_sequence,
        view.config_hash,
    ))
}
pub(super) fn outbox_response(
    view: &OutputView,
    after: u64,
) -> std::result::Result<OutboxResponse, FetchError> {
    let oldest = view
        .retained
        .first()
        .map(|event| event.system().sequence())
        .unwrap_or(view.snapshot.as_of_sequence);
    if after > view.snapshot.as_of_sequence
        || after < view.snapshot.as_of_sequence
            && (view.retained.is_empty() || after < oldest.saturating_sub(1))
    {
        return Err(FetchError::OutboxGap(OutboxGap {
            requested: after,
            earliest_available: oldest,
            latest_sequence: view.snapshot.as_of_sequence,
            config_hash: view.config_hash,
        }));
    }
    let results = view
        .retained
        .iter()
        .filter(|event| event.system().sequence() > after)
        .map(|event| {
            QueryChangeCodec::to_legacy_result(event)
                .map(Arc::new)
                .map_err(|error| {
                    log::error!(
                        "Cannot decode native query history for a legacy reaction: {error}"
                    );
                    FetchError::NotRunning {
                        status: ComponentStatus::Error,
                    }
                })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(OutboxResponse {
        results,
        latest_sequence: view.snapshot.as_of_sequence,
        config_hash: view.config_hash,
    })
}

struct Fetcher {
    catalog: QueryResultsCatalog,
    queries: BTreeSet<String>,
    timeout: Duration,
}
#[async_trait]
impl SnapshotFetcher for Fetcher {
    async fn fetch_snapshot(&self, id: &str) -> std::result::Result<SnapshotStream, FetchError> {
        if !self.queries.contains(id) {
            return Err(FetchError::NotRunning {
                status: ComponentStatus::Error,
            });
        }
        let query = self
            .catalog
            .ready(id, self.timeout)
            .await
            .map_err(|error| {
                log::warn!("Native query {id} cannot supply a legacy snapshot: {error:#}");
                if error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
                {
                    FetchError::TimedOut
                } else {
                    FetchError::NotRunning {
                        status: ComponentStatus::Error,
                    }
                }
            })?;
        let view = output_view(&query).map_err(|_| FetchError::NotRunning {
            status: ComponentStatus::Error,
        })?;
        snapshot_response(id, &view)
            .map(SnapshotStream::from_snapshot)
            .map_err(|error| {
                log::error!("Native snapshot conversion failed: {error:#}");
                FetchError::NotRunning {
                    status: ComponentStatus::Error,
                }
            })
    }
}

struct BootstrapView {
    query: String,
    view: OutputView,
    original: Option<ReactionCheckpoint>,
    staged: Arc<Mutex<Option<ReactionCheckpoint>>>,
}
#[async_trait]
impl BootstrapBackend for BootstrapView {
    async fn fetch_snapshot(&self) -> std::result::Result<SnapshotStream, FetchError> {
        snapshot_response(&self.query, &self.view)
            .map(SnapshotStream::from_snapshot)
            .map_err(|error| {
                log::error!("Native bootstrap snapshot conversion failed: {error:#}");
                FetchError::NotRunning {
                    status: ComponentStatus::Error,
                }
            })
    }
    async fn fetch_outbox(&self, after: u64) -> std::result::Result<OutboxStream, FetchError> {
        outbox_response(&self.view, after).map(OutboxStream::from_outbox)
    }
    async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>> {
        Ok(self
            .staged
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap checkpoint ownership poisoned"))?
            .clone()
            .or_else(|| self.original.clone()))
    }
    async fn write_checkpoint(&self, checkpoint: &ReactionCheckpoint) -> anyhow::Result<()> {
        if checkpoint.sequence > self.view.snapshot.as_of_sequence
            || checkpoint.config_hash != self.view.config_hash
        {
            anyhow::bail!("bootstrap checkpoint is outside its frozen query view");
        }
        *self
            .staged
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap checkpoint ownership poisoned"))? =
            Some(checkpoint.clone());
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RecoveryMetadata {
    config_hash: u64,
    reset_generation: u64,
    bootstrap_pending: bool,
    #[serde(default)]
    incarnation: Option<uuid::Uuid>,
}
#[derive(Default)]
struct ReactionLife {
    initialized: bool,
    initialization_complete: bool,
    needs_stop: bool,
    running: bool,
    replace_on_start: bool,
    closed: bool,
    accepted: BTreeMap<String, (u64, u64)>,
}

pub struct ReactionPluginHost {
    id: String,
    query_ids: Vec<String>,
    reaction: Mutex<Arc<dyn Reaction>>,
    constructor: Option<Arc<dyn ReactionPluginConstructor>>,
    services: LegacyPluginServices,
    catalog: QueryResultsCatalog,
    options: ReactionPluginOptions,
    owned: bool,
    claimed: AtomicBool,
    life: tokio::sync::Mutex<ReactionLife>,
    observations: PluginObservations,
    deferred_validation: bool,
    runtime_metrics: Option<RuntimeReactionMetrics>,
}

pub(crate) struct RuntimeReactionMetrics {
    pub queries: BTreeMap<String, Arc<crate::metrics::ReactionMetrics>>,
    pub lifecycle: Arc<crate::metrics::LifecycleMetrics>,
}

impl ReactionPluginHost {
    pub fn owned(
        reaction: Box<dyn Reaction>,
        services: LegacyPluginServices,
        catalog: QueryResultsCatalog,
        options: ReactionPluginOptions,
    ) -> anyhow::Result<Arc<Self>> {
        Self::new(
            reaction.into(),
            services,
            catalog,
            options,
            true,
            None,
            false,
        )
    }
    pub fn borrowed(
        reaction: Arc<dyn Reaction>,
        catalog: QueryResultsCatalog,
    ) -> anyhow::Result<Arc<Self>> {
        Self::new(
            reaction,
            LegacyPluginServices::empty("borrowed"),
            catalog,
            ReactionPluginOptions::default(),
            false,
            None,
            false,
        )
    }
    pub async fn recreatable(
        constructor: Arc<dyn ReactionPluginConstructor>,
        services: LegacyPluginServices,
        catalog: QueryResultsCatalog,
        options: ReactionPluginOptions,
    ) -> anyhow::Result<Arc<Self>> {
        let reaction = constructor.create().await?;
        Self::new(
            reaction.into(),
            services,
            catalog,
            options,
            true,
            Some(constructor),
            false,
        )
    }
    pub(crate) fn for_runtime(
        reaction: Arc<dyn Reaction>,
        services: LegacyPluginServices,
        catalog: QueryResultsCatalog,
        options: ReactionPluginOptions,
        metrics: RuntimeReactionMetrics,
    ) -> anyhow::Result<Arc<Self>> {
        let mut host = Self::new(reaction, services, catalog, options, true, None, true)?;
        Arc::get_mut(&mut host).expect("new host").runtime_metrics = Some(metrics);
        Ok(host)
    }
    fn new(
        reaction: Arc<dyn Reaction>,
        services: LegacyPluginServices,
        catalog: QueryResultsCatalog,
        options: ReactionPluginOptions,
        owned: bool,
        constructor: Option<Arc<dyn ReactionPluginConstructor>>,
        deferred_validation: bool,
    ) -> anyhow::Result<Arc<Self>> {
        if options.bootstrap_timeout_secs == 0 {
            anyhow::bail!("reaction bootstrap timeout must be nonzero");
        }
        if owned
            && !deferred_validation
            && reaction.is_durable()
            && services
                .state_store
                .as_ref()
                .map_or(true, |store| !store.is_durable())
        {
            anyhow::bail!("durable reaction requires a durable state store");
        }
        let policy = options
            .recovery
            .unwrap_or_else(|| reaction.default_recovery_policy());
        if owned
            && !deferred_validation
            && policy == ReactionRecoveryPolicy::AutoReset
            && !reaction.needs_snapshot_on_fresh_start()
        {
            anyhow::bail!("automatic reaction reset requires snapshot/bootstrap support");
        }
        let query_ids = reaction.query_ids();
        let mut unique = BTreeSet::new();
        for id in &query_ids {
            ComponentId::try_new(id.as_str())?;
            if !unique.insert(id) {
                anyhow::bail!("reaction has duplicate query IDs");
            }
        }
        let id = reaction.id().to_owned();
        Ok(Arc::new(Self {
            observations: PluginObservations::new(&id),
            id,
            query_ids,
            reaction: Mutex::new(reaction),
            constructor,
            services,
            catalog,
            options,
            owned,
            claimed: AtomicBool::new(false),
            life: tokio::sync::Mutex::new(ReactionLife::default()),
            deferred_validation,
            runtime_metrics: None,
        }))
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_owned(&self) -> bool {
        self.owned
    }
    pub fn query_ids(&self) -> &[String] {
        &self.query_ids
    }
    pub fn services(&self) -> &LegacyPluginServices {
        &self.services
    }
    pub fn catalog(&self) -> &QueryResultsCatalog {
        &self.catalog
    }
    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        let resource = ResourceHandle::new(ResourceRole::LegacyReaction, self.clone());
        if self.owned {
            resource.with_cleanup(self.clone())
        } else {
            resource
        }
    }
    fn reaction(&self) -> anyhow::Result<Arc<dyn Reaction>> {
        Ok(self
            .reaction
            .lock()
            .map_err(|_| anyhow::anyhow!("reaction instance ownership poisoned"))?
            .clone())
    }
    fn metadata_key(&self, query: &str) -> String {
        format!(
            "computation-query:{}",
            query
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }
    async fn read_metadata(&self, query: &str) -> anyhow::Result<Option<RecoveryMetadata>> {
        if let Some(store) = &self.services.state_store {
            store
                .get(&self.id, &self.metadata_key(query))
                .await?
                .map(|value| serde_json::from_slice(&value).map_err(Into::into))
                .transpose()
        } else {
            Ok(None)
        }
    }
    async fn write_metadata(
        &self,
        query: &str,
        view: &OutputView,
        pending: bool,
    ) -> anyhow::Result<()> {
        if let Some(store) = &self.services.state_store {
            store
                .set(
                    &self.id,
                    &self.metadata_key(query),
                    serde_json::to_vec(&RecoveryMetadata {
                        config_hash: view.config_hash,
                        reset_generation: view.snapshot.generation,
                        bootstrap_pending: pending,
                        incarnation: view.incarnation,
                    })?,
                )
                .await?;
        }
        Ok(())
    }
    async fn bootstrap(
        &self,
        reaction: &dyn Reaction,
        query: &str,
        view: &OutputView,
        checkpoint: Option<ReactionCheckpoint>,
        reset: bool,
    ) -> anyhow::Result<()> {
        if let Some(metrics) = &self.runtime_metrics {
            if let Some(query) = metrics.queries.get(query) {
                query.record_fetch_snapshot();
            }
        }
        self.write_metadata(query, view, true).await?;
        let staged = Arc::new(Mutex::new(None));
        let context = BootstrapContext::from_backend(
            query.to_owned(),
            reset,
            Box::new(BootstrapView {
                query: query.to_owned(),
                view: view.clone(),
                original: checkpoint,
                staged: staged.clone(),
            }),
        );
        tokio::time::timeout(
            Duration::from_secs(self.options.bootstrap_timeout_secs),
            self.observations.drive(reaction.bootstrap(context), false),
        )
        .await??;
        if let Some(store) = &self.services.state_store {
            let checkpoint = ReactionCheckpoint {
                sequence: view.snapshot.as_of_sequence,
                config_hash: view.config_hash,
            };
            crate::reactions::checkpoint::write_checkpoint(
                store.as_ref(),
                &self.id,
                query,
                &checkpoint,
            )
            .await?;
        }
        self.write_metadata(query, view, false).await?;
        if reset {
            if let Some(metrics) = &self.runtime_metrics {
                metrics.lifecycle.record_auto_reset_completion();
            }
        }
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.closed {
            anyhow::bail!("reaction host is closed");
        }
        if life.running {
            return Ok(());
        }
        if !self.owned {
            life.running = true;
            return Ok(());
        }
        if self.deferred_validation {
            let reaction = self.reaction()?;
            let policy = self
                .options
                .recovery
                .unwrap_or_else(|| reaction.default_recovery_policy());
            if reaction.is_durable()
                && self
                    .services
                    .state_store
                    .as_ref()
                    .map_or(true, |store| !store.is_durable())
            {
                if let Some(metrics) = &self.runtime_metrics {
                    metrics.lifecycle.record_startup_rejection(
                        if self.services.state_store.is_some() {
                            crate::metrics::StartupRejectionReason::DurableOnVolatile
                        } else {
                            crate::metrics::StartupRejectionReason::DurableNoStore
                        },
                    );
                }
                anyhow::bail!("durable reaction requires a durable state store");
            }
            if reaction.needs_snapshot_on_fresh_start()
                && policy == ReactionRecoveryPolicy::AutoSkipGap
            {
                if let Some(metrics) = &self.runtime_metrics {
                    metrics.lifecycle.record_startup_rejection(
                        crate::metrics::StartupRejectionReason::SnapshotSkipGap,
                    );
                }
                anyhow::bail!("snapshot recovery is incompatible with AutoSkipGap");
            }
            if !reaction.needs_snapshot_on_fresh_start()
                && policy == ReactionRecoveryPolicy::AutoReset
            {
                if let Some(metrics) = &self.runtime_metrics {
                    metrics.lifecycle.record_startup_rejection(
                        crate::metrics::StartupRejectionReason::NoSnapshotAutoReset,
                    );
                }
                anyhow::bail!("needs_snapshot_on_fresh_start=false is incompatible with AutoReset (snapshot/bootstrap support is required)");
            }
        }
        if life.replace_on_start {
            let reaction = self
                .constructor
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing reaction reconstruction factory"))?
                .create()
                .await?;
            if reaction.id() != self.id || reaction.query_ids() != self.query_ids {
                anyhow::bail!("reaction reconstruction changed identity or query bindings");
            }
            let previous = self.reaction()?;
            if reaction.is_durable() != previous.is_durable()
                || reaction.needs_snapshot_on_fresh_start()
                    != previous.needs_snapshot_on_fresh_start()
                || reaction.default_recovery_policy() != previous.default_recovery_policy()
            {
                anyhow::bail!("reaction reconstruction changed its recovery contract");
            }
            *self
                .reaction
                .lock()
                .map_err(|_| anyhow::anyhow!("reaction ownership poisoned"))? = reaction.into();
            life.initialized = false;
            life.initialization_complete = false;
            life.replace_on_start = false;
        }
        let reaction = self.initialize_locked(&mut life).await?;
        self.observations.reset().await;
        if !self.deferred_validation {
            for query in &self.query_ids {
                self.catalog
                    .ready(
                        query,
                        Duration::from_secs(self.options.bootstrap_timeout_secs),
                    )
                    .await?;
            }
        }
        life.needs_stop = true;
        self.observations.drive(reaction.start(), false).await?;
        life.accepted.clear();
        for query_id in &self.query_ids {
            let checkpoint = if let Some(store) = &self.services.state_store {
                crate::reactions::checkpoint::read_checkpoint(store.as_ref(), &self.id, query_id)
                    .await?
            } else {
                None
            };
            if self.deferred_validation
                && checkpoint.is_none()
                && !reaction.needs_snapshot_on_fresh_start()
            {
                if let Some(query) = self
                    .catalog
                    .get(&ComponentId::try_new(query_id.as_str())?)?
                {
                    if query.status.as_ref().is_some_and(|status| {
                        !matches!(
                            *status.borrow(),
                            ComponentStatus::Starting | ComponentStatus::Running
                        )
                    }) {
                        let view = output_view(&query)?;
                        if let Some(metrics) = self
                            .runtime_metrics
                            .as_ref()
                            .and_then(|metrics| metrics.queries.get(query_id))
                        {
                            metrics.record_fetch_outbox();
                        }
                        if let Some(store) = &self.services.state_store {
                            crate::reactions::checkpoint::write_checkpoint(
                                store.as_ref(),
                                &self.id,
                                query_id,
                                &ReactionCheckpoint {
                                    sequence: 0,
                                    config_hash: view.config_hash,
                                },
                            )
                            .await?;
                        }
                        life.accepted
                            .insert(query_id.clone(), (view.snapshot.generation, 0));
                        self.write_metadata(query_id, &view, false).await?;
                        continue;
                    }
                }
            }
            let query = self
                .catalog
                .ready(
                    query_id,
                    Duration::from_secs(self.options.bootstrap_timeout_secs),
                )
                .await?;
            let view = output_view(&query)?;
            let metadata = self.read_metadata(query_id).await?;
            if checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.config_hash != view.config_hash)
            {
                if let Some(metrics) = &self.runtime_metrics {
                    metrics.lifecycle.record_hash_mismatch();
                }
            }
            let changed = metadata.as_ref().is_some_and(|metadata| {
                metadata.bootstrap_pending
                    || metadata.config_hash != view.config_hash
                    || metadata.reset_generation != view.snapshot.generation
                    || metadata.incarnation != view.incarnation
            }) || checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.sequence > view.snapshot.as_of_sequence)
                || metadata.is_none()
                    && checkpoint.as_ref().is_some_and(|checkpoint| {
                        checkpoint.config_hash != view.config_hash
                            || !self.deferred_validation && view.incarnation.is_some()
                    });
            let policy = self
                .options
                .recovery
                .unwrap_or_else(|| reaction.default_recovery_policy());
            let needs_snapshot = checkpoint.is_none() && reaction.needs_snapshot_on_fresh_start();
            let after = checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.sequence)
                .unwrap_or(0);
            let replay = outbox_response(&view, after);
            if !needs_snapshot {
                if let Some(metrics) = self
                    .runtime_metrics
                    .as_ref()
                    .and_then(|metrics| metrics.queries.get(query_id))
                {
                    metrics.record_fetch_outbox();
                    if matches!(&replay, Err(FetchError::OutboxGap(_))) {
                        metrics.record_gap_detection();
                    }
                }
            }
            if needs_snapshot
                || changed && policy == ReactionRecoveryPolicy::AutoReset
                || replay.is_err() && policy == ReactionRecoveryPolicy::AutoReset
            {
                if !reaction.needs_snapshot_on_fresh_start() {
                    anyhow::bail!("reaction cannot reset from a snapshot");
                }
                let reset = changed || checkpoint.is_some();
                self.bootstrap(reaction.as_ref(), query_id, &view, checkpoint, reset)
                    .await?;
                life.accepted.insert(
                    query_id.clone(),
                    (view.snapshot.generation, view.snapshot.as_of_sequence),
                );
                continue;
            }
            if changed && policy != ReactionRecoveryPolicy::AutoReset {
                if self.deferred_validation {
                    anyhow::bail!("{policy:?} recovery policy cannot recover a checkpoint from a different query/reset generation");
                }
                anyhow::bail!("reaction checkpoint belongs to a different query/reset generation");
            }
            let entries = match replay {
                Ok(replay) => replay.results,
                Err(FetchError::OutboxGap(_))
                    if policy == ReactionRecoveryPolicy::AutoSkipGap
                        || checkpoint.is_none() && !reaction.needs_snapshot_on_fresh_start() =>
                {
                    log::warn!("Reaction {} starts at available retained history for query {query_id}; missing history is not claimed handled", self.id);
                    view.retained
                        .iter()
                        .map(|event| QueryChangeCodec::to_legacy_result(event).map(Arc::new))
                        .collect::<std::result::Result<Vec<_>, _>>()?
                }
                Err(error) => return Err(error.into()),
            };
            let mut accepted = after;
            for result in entries {
                self.observations
                    .drive(
                        reaction.enqueue_query_result(result.as_ref().clone()),
                        false,
                    )
                    .await?;
                accepted = result.sequence;
            }
            // This is delivery dedup within this run, never a handled checkpoint.
            life.accepted
                .insert(query_id.clone(), (view.snapshot.generation, accepted));
            if self.deferred_validation {
                if let Some(store) = &self.services.state_store {
                    crate::reactions::checkpoint::write_checkpoint(
                        store.as_ref(),
                        &self.id,
                        query_id,
                        &ReactionCheckpoint {
                            sequence: accepted,
                            config_hash: view.config_hash,
                        },
                    )
                    .await?;
                }
            }
            self.write_metadata(query_id, &view, false).await?;
        }
        life.running = true;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if self.owned && life.needs_stop {
            self.observations
                .drive(self.reaction()?.stop(), true)
                .await?;
            life.needs_stop = false;
            life.replace_on_start = self.constructor.is_some();
        }
        life.running = false;
        Ok(())
    }
    pub(crate) async fn initialize_component(&self) -> anyhow::Result<()> {
        if !self.owned {
            anyhow::bail!("borrowed reactions cannot be initialized");
        }
        let mut life = self.life.lock().await;
        self.initialize_locked(&mut life).await?;
        Ok(())
    }
    async fn initialize_locked(
        &self,
        life: &mut ReactionLife,
    ) -> anyhow::Result<Arc<dyn Reaction>> {
        let reaction = self.reaction()?;
        if life.initialized && !life.initialization_complete {
            anyhow::bail!("reaction initialization was interrupted; reconstruct it");
        }
        if !life.initialized {
            life.initialized = true;
            life.needs_stop = true;
            let mut context = ReactionRuntimeContext::new(
                self.services.scope.as_ref(),
                &self.id,
                self.services.state_store.clone(),
                self.observations.channel().await,
                self.services.identity.clone(),
            );
            context.snapshot_fetcher = Some(Arc::new(Fetcher {
                catalog: self.catalog.clone(),
                queries: self.query_ids.iter().cloned().collect(),
                timeout: Duration::from_secs(self.options.bootstrap_timeout_secs),
            }));
            self.observations
                .drive(
                    async {
                        reaction.initialize(context).await;
                        Ok(())
                    },
                    false,
                )
                .await?;
            life.initialization_complete = true;
        }
        Ok(reaction)
    }
    async fn accept(&self, envelope: &ChangeEnvelope) -> anyhow::Result<()> {
        if QueryChangeCodec::is_snapshot(envelope) {
            anyhow::bail!(
                "reaction snapshot recovery uses its bootstrap callback, not queue acceptance"
            );
        }
        let result = QueryChangeCodec::to_legacy_result(envelope)?;
        if !self.query_ids.contains(&result.query_id) {
            anyhow::bail!("reaction does not subscribe to this query");
        }
        let generation = QueryChangeCodec::query_generation(envelope)?;
        let mut life = self.life.lock().await;
        if !life.running {
            anyhow::bail!("reaction is not activated");
        }
        if let Some((previous_generation, sequence)) = life.accepted.get(&result.query_id) {
            if generation < *previous_generation {
                anyhow::bail!("obsolete query reset generation");
            }
            if generation == *previous_generation && result.sequence <= *sequence {
                if let Some(metrics) = self
                    .runtime_metrics
                    .as_ref()
                    .and_then(|metrics| metrics.queries.get(&result.query_id))
                {
                    metrics.record_dedup_skip();
                }
                return Ok(());
            }
            if generation != *previous_generation {
                anyhow::bail!("query generation changed; restart reaction recovery");
            }
        }
        self.observations
            .drive(self.reaction()?.enqueue_query_result(result.clone()), false)
            .await?;
        if let Some(metrics) = self
            .runtime_metrics
            .as_ref()
            .and_then(|metrics| metrics.queries.get(&result.query_id))
        {
            metrics.record_checkpoint(
                result.sequence,
                self.catalog.current_sequence(&result.query_id)?,
            );
        }
        life.accepted
            .insert(result.query_id, (generation, result.sequence));
        Ok(())
    }
    pub async fn deprovision_owned(&self) -> anyhow::Result<()> {
        if !self.owned {
            anyhow::bail!("cannot deprovision a borrowed reaction");
        }
        self.shutdown().await?;
        self.reaction()?.deprovision().await
    }
    pub(crate) async fn start_component(&self) -> anyhow::Result<()> {
        self.start().await
    }
    pub(crate) async fn stop_component(&self) -> anyhow::Result<()> {
        self.stop().await
    }
    pub(crate) async fn run_observations(&self) -> anyhow::Result<()> {
        self.observations.drive(std::future::pending(), false).await
    }
    pub(crate) async fn handle_envelope(&self, envelope: &ChangeEnvelope) -> anyhow::Result<()> {
        self.accept(envelope).await
    }
    pub(crate) async fn recover_gap(&self, query: &str) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        let reaction = self.reaction()?;
        let policy = self
            .options
            .recovery
            .unwrap_or_else(|| reaction.default_recovery_policy());
        if let Some(metrics) = self
            .runtime_metrics
            .as_ref()
            .and_then(|metrics| metrics.queries.get(query))
        {
            metrics.record_gap_detection();
            metrics.record_recovery_trigger(match policy {
                ReactionRecoveryPolicy::Strict => crate::metrics::RecoveryPolicyKind::Strict,
                ReactionRecoveryPolicy::AutoReset => crate::metrics::RecoveryPolicyKind::AutoReset,
                ReactionRecoveryPolicy::AutoSkipGap => {
                    crate::metrics::RecoveryPolicyKind::AutoSkipGap
                }
            });
        }
        if policy == ReactionRecoveryPolicy::Strict {
            anyhow::bail!("Strict recovery policy cannot recover a live query output gap");
        }
        let entry = self
            .catalog
            .ready(
                query,
                Duration::from_secs(self.options.bootstrap_timeout_secs),
            )
            .await?;
        let view = output_view(&entry)?;
        let checkpoint = life
            .accepted
            .get(query)
            .map(|(_, sequence)| ReactionCheckpoint {
                sequence: *sequence,
                config_hash: view.config_hash,
            });
        if policy == ReactionRecoveryPolicy::AutoReset {
            self.bootstrap(reaction.as_ref(), query, &view, checkpoint, true)
                .await?;
        } else if let Some(store) = &self.services.state_store {
            crate::reactions::checkpoint::write_checkpoint(
                store.as_ref(),
                &self.id,
                query,
                &ReactionCheckpoint {
                    sequence: view.snapshot.as_of_sequence,
                    config_hash: view.config_hash,
                },
            )
            .await?;
        }
        life.accepted.insert(
            query.to_owned(),
            (view.snapshot.generation, view.snapshot.as_of_sequence),
        );
        self.write_metadata(query, &view, false).await
    }
}
#[async_trait]
impl ResourceCleanup for ReactionPluginHost {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop().await?;
        self.life.lock().await.closed = true;
        self.observations.close().await;
        Ok(())
    }
}

pub struct ReactionPluginAdapter {
    descriptor: ComponentDescriptor,
    host: Arc<ReactionPluginHost>,
}
impl ReactionPluginAdapter {
    pub fn new(id: ComponentId, host: Arc<ReactionPluginHost>) -> anyhow::Result<Self> {
        if host.claimed.swap(true, Ordering::AcqRel) {
            anyhow::bail!("reaction host already has a graph consumer");
        }
        Ok(Self {
            descriptor: ComponentDescriptor::try_new(
                id,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            host,
        })
    }
}
impl Drop for ReactionPluginAdapter {
    fn drop(&mut self) {
        self.host.claimed.store(false, Ordering::Release);
    }
}
#[async_trait]
impl ComputationComponent for ReactionPluginAdapter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.host.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.host.stop().await
    }
}
#[async_trait]
impl EnvelopeSink for ReactionPluginAdapter {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.host.accept(&input.envelope).await
    }
}

pub struct ReactionPluginAdapterFactory {
    descriptor: FactoryDescriptor,
}
impl Default for ReactionPluginAdapterFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/reaction-plugin", "1")
                    .expect("implementation"),
                role: ComponentRole::Sink,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: [
                    (
                        Arc::from("reaction"),
                        ResourceRequirement::exactly_one::<ReactionPluginHost>(
                            ResourceRole::LegacyReaction,
                        ),
                    ),
                    (
                        Arc::from("catalog"),
                        ResourceRequirement::exactly_one::<QueryResultsCatalog>(
                            ResourceRole::QueryCatalog,
                        ),
                    ),
                ]
                .into_iter()
                .chain(LegacyPluginServices::requirements())
                .collect(),
            },
        }
    }
}
#[async_trait]
impl ComponentFactory for ReactionPluginAdapterFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let expected = ComponentDescriptor::try_new(
            spec.descriptor.id().clone(),
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        if spec.descriptor != expected || spec.completion != Some(SinkCompletion::Accepted) {
            anyhow::bail!(
                "legacy reaction requires typed query results and acceptance-only completion"
            );
        }
        Ok(())
    }
    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        if let Some(id) = spec
            .dependencies
            .get("reaction")
            .and_then(|ids| ids.first())
        {
            if let Some(handle) = resources.get(id) {
                let host = handle.get::<ReactionPluginHost>()?;
                if host.owned != (declarations[id].ownership == ResourceOwnership::Graph) {
                    anyhow::bail!("reaction lifecycle ownership differs from resource declaration");
                }
                if host.owned && !handle.has_cleanup() {
                    anyhow::bail!("owned reaction requires a cleanup owner");
                }
                host.services.validate_bindings(spec, resources)?;
                if let Some(catalog) = spec
                    .dependencies
                    .get("catalog")
                    .and_then(|ids| ids.first())
                    .and_then(|id| resources.get(id))
                {
                    if !host
                        .catalog
                        .same_registry(catalog.get::<QueryResultsCatalog>()?.as_ref())
                    {
                        anyhow::bail!("reaction query catalogue mismatch");
                    }
                }
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let host = context
            .resources::<ReactionPluginHost>("reaction")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing reaction host"))
            })?;
        let catalog = context
            .resources::<QueryResultsCatalog>("catalog")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing query catalogue"))
            })?;
        if !host.catalog.same_registry(&catalog) {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "reaction query catalogue mismatch"
            )));
        }
        Ok(ConstructedComponent::sink(Box::new(
            ReactionPluginAdapter::new(context.component_id, host)
                .map_err(ComponentCreationError::terminal)?,
        )))
    }
}
