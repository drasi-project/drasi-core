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
        )
        .with_output_generation(view.snapshot.generation));
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
    Ok(
        SnapshotResponse::new(rows, view.snapshot.as_of_sequence, view.config_hash)
            .with_output_generation(view.snapshot.generation),
    )
}

fn snapshot_stream(
    query_id: &str,
    snapshot: QuerySnapshot,
    config_hash: u64,
) -> anyhow::Result<SnapshotStream> {
    let rows = QueryChangeCodec::legacy_snapshot_rows(query_id, snapshot.rows)?;
    Ok(SnapshotStream::from_keyed_stream(
        tokio_stream::iter(rows),
        snapshot.as_of_sequence,
        config_hash,
    ))
}

pub(super) fn outbox_response(
    view: &OutputView,
    after: u64,
) -> std::result::Result<OutboxResponse, FetchError> {
    if after == u64::MAX {
        return Ok(OutboxResponse {
            results: Vec::new(),
            latest_sequence: view.snapshot.as_of_sequence,
            config_hash: view.config_hash,
            output_generation: view.snapshot.generation,
        });
    }
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
        output_generation: view.snapshot.generation,
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
        let snapshot = query
            .results
            .snapshot()
            .map_err(|_| FetchError::NotRunning {
                status: ComponentStatus::Error,
            })?;
        snapshot_stream(id, snapshot, query.config_hash).map_err(|error| {
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
        snapshot_stream(
            &self.query,
            self.view.snapshot.clone(),
            self.view.config_hash,
        )
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
    // Plugin checkpoints preserve the snapshot's hash; native epoch fences live alongside it.
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
    query_generations: BTreeMap<String, u64>,
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
    resource_observer: Mutex<Option<Arc<dyn crate::context::ComponentResourceObserver>>>,
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
            && policy == ReactionRecoveryPolicy::AutoSkipGap
            && reaction.needs_snapshot_on_fresh_start()
        {
            anyhow::bail!("snapshot recovery is incompatible with AutoSkipGap");
        }
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
            resource_observer: Mutex::new(None),
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

    fn reject_startup(&self, reason: crate::metrics::StartupRejectionReason) {
        if let Some(metrics) = &self.runtime_metrics {
            metrics.lifecycle.record_startup_rejection(reason);
        }
    }

    async fn validate_startup(&self, reaction: &dyn Reaction) -> anyhow::Result<()> {
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
            let reason = if self.services.state_store.is_some() {
                self.reject_startup(crate::metrics::StartupRejectionReason::DurableOnVolatile);
                "the configured state store is volatile"
            } else {
                self.reject_startup(crate::metrics::StartupRejectionReason::DurableNoStore);
                "no state store is configured"
            };
            anyhow::bail!(
                "Reaction '{}' requires a durable state store (is_durable=true), but {reason}",
                self.id
            );
        }
        if reaction.is_durable() {
            for query in &self.query_ids {
                self.validate_output_persistence(query).await?;
            }
        }
        if reaction.needs_snapshot_on_fresh_start() && policy == ReactionRecoveryPolicy::AutoSkipGap
        {
            self.reject_startup(crate::metrics::StartupRejectionReason::SnapshotSkipGap);
            anyhow::bail!("snapshot recovery is incompatible with AutoSkipGap");
        }
        if !reaction.needs_snapshot_on_fresh_start() && policy == ReactionRecoveryPolicy::AutoReset
        {
            self.reject_startup(crate::metrics::StartupRejectionReason::NoSnapshotAutoReset);
            anyhow::bail!("needs_snapshot_on_fresh_start=false is incompatible with AutoReset (snapshot/bootstrap support is required)");
        }
        Ok(())
    }

    async fn validate_output_persistence(&self, query: &str) -> anyhow::Result<()> {
        // An active query may still be constructing its stores. Wait only when
        // the capability is unknown, never before validating the state store.
        if self.catalog.output_is_persistent(query).is_err() {
            self.wait_query_ready(query).await.map_err(|error| {
                error.context(format!(
                    "cannot verify durable output for reaction '{}' query '{query}'",
                    self.id
                ))
            })?;
        }
        let persistent = self.catalog.output_is_persistent(query).map_err(|error| {
            error.context(format!(
                "cannot verify durable output for reaction '{}' query '{query}'",
                self.id
            ))
        })?;
        if !persistent {
            self.reject_startup(crate::metrics::StartupRejectionReason::DurableOnVolatileQuery);
            anyhow::bail!(
                "Reaction '{}' requires a persistent query (is_durable=true), but subscribed query '{query}' has volatile output",
                self.id
            );
        }
        Ok(())
    }

    fn validate_head(
        query_id: &str,
        head: &QuerySubscriptionHead,
        query: &CatalogQuery,
        view: &OutputView,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            head.belongs_to(query)
                && head.generation == view.snapshot.generation
                && head.config_hash == view.config_hash
                && head.incarnation == view.incarnation
                && head.sequence <= view.snapshot.as_of_sequence,
            "query '{query_id}' changed after its reaction subscription was attached"
        );
        Ok(())
    }

    async fn checkpoint(&self, query: &str) -> anyhow::Result<Option<ReactionCheckpoint>> {
        match &self.services.state_store {
            Some(store) => {
                crate::reactions::checkpoint::read_checkpoint(store.as_ref(), &self.id, query).await
            }
            None => Ok(None),
        }
    }

    async fn seed_position(
        &self,
        query: &str,
        sequence: u64,
        view: &OutputView,
    ) -> anyhow::Result<()> {
        // Only fresh-start/explicit-skip cutoffs belong here, never enqueue progress.
        self.write_metadata(query, view, false).await?;
        if let Some(store) = &self.services.state_store {
            crate::reactions::checkpoint::write_checkpoint(
                store.as_ref(),
                &self.id,
                query,
                &ReactionCheckpoint {
                    sequence,
                    config_hash: view.config_hash,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.start_with_heads(None).await
    }

    async fn start_with_heads(
        &self,
        mut supplied_heads: Option<BTreeMap<String, QuerySubscriptionHead>>,
    ) -> anyhow::Result<()> {
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
        self.validate_startup(self.reaction()?.as_ref()).await?;
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
        let mut heads = BTreeMap::new();
        for query_id in &self.query_ids {
            let query = self
                .catalog
                .ready(
                    query_id,
                    Duration::from_secs(self.options.bootstrap_timeout_secs),
                )
                .await?;
            if reaction.is_durable() {
                self.validate_output_persistence(query_id).await?;
            }
            let head = match &mut supplied_heads {
                Some(heads) => heads.remove(query_id).ok_or_else(|| {
                    anyhow::anyhow!("query '{query_id}' has no subscription head")
                })?,
                None => self.catalog.query_head(query_id)?,
            };
            Self::validate_head(query_id, &head, &query, &output_view(&query)?)?;
            heads.insert(query_id.clone(), head);
        }
        anyhow::ensure!(
            supplied_heads.as_ref().map_or(true, BTreeMap::is_empty),
            "reaction received subscription heads for unrelated queries"
        );
        life.needs_stop = true;
        self.observations.drive(reaction.start(), false).await?;
        life.accepted.clear();
        life.query_generations.clear();
        for query_id in &self.query_ids {
            let query = self
                .catalog
                .ready(
                    query_id,
                    Duration::from_secs(self.options.bootstrap_timeout_secs),
                )
                .await?;
            let view = output_view(&query)?;
            let head = &heads[query_id];
            Self::validate_head(query_id, head, &query, &view)?;
            life.query_generations
                .insert(query_id.clone(), query.generation);
            let checkpoint = self.checkpoint(query_id).await?;
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
            if needs_snapshot || changed && policy == ReactionRecoveryPolicy::AutoReset {
                let reset = changed || checkpoint.is_some();
                self.bootstrap(reaction.as_ref(), query_id, &view, checkpoint, reset)
                    .await?;
                life.accepted.insert(
                    query_id.clone(),
                    (view.snapshot.generation, view.snapshot.as_of_sequence),
                );
                continue;
            }
            if changed {
                if policy != ReactionRecoveryPolicy::AutoSkipGap {
                    anyhow::bail!("{policy:?} recovery policy cannot recover a checkpoint from a different query/reset generation");
                }
                log::warn!(
                    "Reaction {} skips obsolete query identity or checkpoint for query {query_id} per AutoSkipGap",
                    self.id
                );
                // A missing checkpoint is still a fresh trigger: do not skip
                // arrivals after its subscription attached during start().
                let sequence = if checkpoint.is_none() {
                    head.sequence
                } else {
                    view.snapshot.as_of_sequence
                };
                self.seed_position(query_id, sequence, &view).await?;
                life.accepted
                    .insert(query_id.clone(), (view.snapshot.generation, sequence));
                continue;
            }
            if checkpoint.is_none() {
                // This is an explicit fresh-start cutoff, not acknowledgement of
                // retained results. Arrivals during start remain above this head.
                self.seed_position(query_id, head.sequence, &view).await?;
                life.accepted
                    .insert(query_id.clone(), (head.generation, head.sequence));
                continue;
            }
            let after = checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.sequence)
                .unwrap_or(0);
            let replay = outbox_response(&view, after);
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
            if replay.is_err() && policy == ReactionRecoveryPolicy::AutoReset {
                self.bootstrap(reaction.as_ref(), query_id, &view, checkpoint, true)
                    .await?;
                life.accepted.insert(
                    query_id.clone(),
                    (view.snapshot.generation, view.snapshot.as_of_sequence),
                );
                continue;
            }
            let entries = match replay {
                Ok(replay) => replay.results,
                Err(FetchError::OutboxGap(_)) if policy == ReactionRecoveryPolicy::AutoSkipGap => {
                    log::warn!("Reaction {} skips unavailable history for query {query_id} per AutoSkipGap", self.id);
                    self.seed_position(query_id, view.snapshot.as_of_sequence, &view)
                        .await?;
                    life.accepted.insert(
                        query_id.clone(),
                        (view.snapshot.generation, view.snapshot.as_of_sequence),
                    );
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            self.write_metadata(query_id, &view, false).await?;
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
        if self.owned {
            life.accepted.clear();
            life.query_generations.clear();
        }
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
    pub(crate) async fn wait_running(&self) -> anyhow::Result<()> {
        if self.reaction()?.status().await == ComponentStatus::Running {
            return Ok(());
        }
        self.observations.wait_running().await
    }
    pub(crate) fn set_resource_observer(
        &self,
        observer: Arc<dyn crate::context::ComponentResourceObserver>,
    ) -> anyhow::Result<()> {
        *self
            .resource_observer
            .lock()
            .map_err(|_| anyhow::anyhow!("reaction resource observer binding poisoned"))? =
            Some(observer);
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
            context.resource_observer = self
                .resource_observer
                .lock()
                .map_err(|_| anyhow::anyhow!("reaction resource observer binding poisoned"))?
                .clone();
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
        let mut recovered_gap = false;
        loop {
            let mut life = self.life.lock().await;
            if !life.running {
                anyhow::bail!("reaction is not activated");
            }
            if self.owned {
                self.require_query_generation(&life, &result.query_id)?;
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
                if self.owned && result.sequence > sequence.saturating_add(1) {
                    anyhow::ensure!(
                        !recovered_gap,
                        "query output is still discontinuous after recovery"
                    );
                    drop(life);
                    self.recover_gap(&result.query_id).await?;
                    recovered_gap = true;
                    continue;
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
                .insert(result.query_id.clone(), (generation, result.sequence));
            return Ok(());
        }
    }

    fn require_query_generation(&self, life: &ReactionLife, query: &str) -> anyhow::Result<()> {
        let current = self
            .catalog
            .get(&ComponentId::try_new(query)?)?
            .ok_or_else(|| anyhow::anyhow!("query '{query}' is no longer registered"))?;
        anyhow::ensure!(
            life.query_generations.get(query) == Some(&current.generation),
            "query '{query}' was replaced; restart the reaction subscription"
        );
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
    pub(crate) async fn validate_startup_configuration(&self) -> anyhow::Result<()> {
        self.validate_startup(self.reaction()?.as_ref()).await
    }
    pub(crate) async fn wait_query_ready(&self, query: &str) -> anyhow::Result<()> {
        self.catalog
            .ready(
                query,
                Duration::from_secs(self.options.bootstrap_timeout_secs),
            )
            .await?;
        Ok(())
    }
    pub(crate) async fn start_component_with_heads(
        &self,
        heads: BTreeMap<String, QuerySubscriptionHead>,
    ) -> anyhow::Result<()> {
        self.start_with_heads(Some(heads)).await
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
        anyhow::ensure!(
            self.owned,
            "borrowed reaction recovery belongs to its owner"
        );
        self.require_query_generation(&life, query)?;
        let reaction = self.reaction()?;
        let policy = self
            .options
            .recovery
            .unwrap_or_else(|| reaction.default_recovery_policy());
        let metrics = self
            .runtime_metrics
            .as_ref()
            .and_then(|metrics| metrics.queries.get(query));
        if let Some(metrics) = metrics {
            metrics.record_gap_detection();
            metrics.record_fetch_outbox();
        }
        let entry = self
            .catalog
            .ready(
                query,
                Duration::from_secs(self.options.bootstrap_timeout_secs),
            )
            .await?;
        let view = output_view(&entry)?;
        self.require_query_generation(&life, query)?;
        let (generation, after) = life.accepted.get(query).copied().ok_or_else(|| {
            anyhow::anyhow!("query '{query}' has no accepted subscription position")
        })?;
        if generation == view.snapshot.generation {
            match outbox_response(&view, after) {
                Ok(replay) => {
                    let mut last = after;
                    let contiguous = replay.results.iter().all(|result| {
                        let next = last.checked_add(1) == Some(result.sequence);
                        last = result.sequence;
                        next
                    }) && last == view.snapshot.as_of_sequence;
                    if contiguous {
                        // Recover transport loss from the last in-memory acceptance.
                        // The reaction alone checkpoints completed side effects.
                        for result in replay.results {
                            self.observations
                                .drive(
                                    reaction.enqueue_query_result(result.as_ref().clone()),
                                    false,
                                )
                                .await?;
                            self.require_query_generation(&life, query)?;
                            life.accepted
                                .insert(query.to_owned(), (generation, result.sequence));
                        }
                        if let Some(metrics) = metrics {
                            metrics.record_checkpoint(last, self.catalog.current_sequence(query)?);
                        }
                        return Ok(());
                    }
                }
                Err(FetchError::OutboxGap(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(metrics) = metrics {
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
        let checkpoint = self.checkpoint(query).await?;
        if policy == ReactionRecoveryPolicy::AutoReset {
            self.bootstrap(reaction.as_ref(), query, &view, checkpoint, true)
                .await?;
        } else {
            self.seed_position(query, view.snapshot.as_of_sequence, &view)
                .await?;
        }
        life.accepted.insert(
            query.to_owned(),
            (view.snapshot.generation, view.snapshot.as_of_sequence),
        );
        Ok(())
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
        self.host.start_component().await
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

#[cfg(test)]
#[path = "plugin_reaction_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::evaluation::variable_value::VariableValue;
    use std::num::NonZeroUsize;

    fn snapshot_view(count: u64) -> OutputView {
        let rows = (1..=count)
            .map(|signature| {
                let row = QueryChangeCodec::encode_row(
                    "query",
                    signature,
                    &BTreeMap::from([
                        ("value".into(), VariableValue::from(signature)),
                        ("payload".into(), VariableValue::from("x".repeat(1024))),
                    ]),
                    QueryRowKind::Row,
                    RecordImage::Full,
                )
                .unwrap();
                (row.identity().clone(), row)
            })
            .collect();
        OutputView {
            snapshot: QuerySnapshot {
                rows,
                as_of_sequence: count + 10,
                generation: 7,
            },
            retained: Vec::new(),
            config_hash: 123,
            incarnation: None,
        }
    }

    #[tokio::test]
    async fn bootstrap_snapshot_projects_only_capped_rows_and_keeps_captured_metadata() {
        let mut backend = BootstrapView {
            query: "query".into(),
            view: snapshot_view(10_000),
            original: None,
            staged: Arc::new(Mutex::new(None)),
        };
        let before = QueryChangeCodec::snapshot_row_projections();
        let empty_cap = backend.fetch_snapshot().await.unwrap();
        assert!(empty_cap.collect_keyed_vec_capped(0).await.is_empty());
        assert_eq!(QueryChangeCodec::snapshot_row_projections(), before);
        let stream = backend.fetch_snapshot().await.unwrap();
        assert_eq!(QueryChangeCodec::snapshot_row_projections(), before);
        backend.view.snapshot.rows.clear();
        backend.view.snapshot.as_of_sequence += 1;
        backend.view.config_hash += 1;
        assert_eq!(stream.as_of_sequence, 10_010);
        assert_eq!(stream.config_hash, 123);
        let rows = stream.collect_keyed_vec_capped(7).await;
        assert_eq!(rows.len(), 7);
        assert_eq!(QueryChangeCodec::snapshot_row_projections() - before, 7);
        for (signature, row) in rows {
            assert_eq!(row["value"], serde_json::json!(signature));
            assert_eq!(row["payload"].as_str().unwrap().len(), 1024);
        }
    }

    #[tokio::test]
    async fn runtime_snapshot_fetcher_projects_only_demanded_rows_from_a_stable_view() {
        let catalog = QueryResultsCatalog::new("snapshot-stream-test").unwrap();
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "snapshot-stream-test".into(),
                id: ComponentId::try_new("query").unwrap(),
                query: "MATCH (n) RETURN n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("query/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(8).unwrap(),
            },
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_result_catalog(&catalog)
        .unwrap();
        query.start().await.unwrap();
        let results = query.results();
        let view = snapshot_view(10_000);
        let identity = results.recovery_view(None).unwrap().identity;
        {
            let mut state = results.state.write().unwrap();
            state.hydrate(
                view.snapshot.rows,
                view.snapshot.as_of_sequence,
                Vec::new(),
                view.snapshot.generation,
                identity,
            );
        }
        let expected_hash = catalog
            .get(&ComponentId::try_new("query").unwrap())
            .unwrap()
            .unwrap()
            .config_hash;
        let fetcher = Fetcher {
            catalog,
            queries: BTreeSet::from(["query".into()]),
            timeout: Duration::from_secs(1),
        };
        let before = QueryChangeCodec::snapshot_row_projections();
        let stream = fetcher.fetch_snapshot("query").await.unwrap();
        assert_eq!(QueryChangeCodec::snapshot_row_projections(), before);
        results.state.write().unwrap().rows.clear();
        assert_eq!(stream.as_of_sequence, 10_010);
        assert_eq!(stream.config_hash, expected_hash);
        let rows = stream.collect_keyed_vec_capped(3).await;
        assert_eq!(rows.len(), 3);
        assert_eq!(QueryChangeCodec::snapshot_row_projections() - before, 3);
        for (signature, row) in rows {
            assert_eq!(row["value"], serde_json::json!(signature));
        }
        let foreign = QueryChangeCodec::encode_row(
            "different-query",
            1,
            &BTreeMap::new(),
            QueryRowKind::Row,
            RecordImage::Full,
        )
        .unwrap();
        results
            .state
            .write()
            .unwrap()
            .rows
            .insert(foreign.identity().clone(), foreign);
        assert!(matches!(
            fetcher.fetch_snapshot("query").await,
            Err(FetchError::NotRunning {
                status: ComponentStatus::Error
            })
        ));
        assert_eq!(QueryChangeCodec::snapshot_row_projections() - before, 3);
        assert!(matches!(
            fetcher.fetch_snapshot("unsubscribed").await,
            Err(FetchError::NotRunning {
                status: ComponentStatus::Error
            })
        ));
        query.stop().await.unwrap();
    }

    struct UncheckedRowValidator;

    impl RecordValidator for UncheckedRowValidator {
        fn validate_identity(
            &self,
            _: &SchemaDescriptor,
            _: &RecordId,
        ) -> std::result::Result<(), RecordValidationError> {
            Ok(())
        }

        fn validate(
            &self,
            _: &SchemaDescriptor,
            _: &RecordId,
            _: RecordImage,
            _: &[u8],
        ) -> std::result::Result<(), RecordValidationError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn snapshot_preflight_rejects_bad_rows_without_hiding_or_panicking() {
        let variables = BTreeMap::from([("value".into(), VariableValue::from(1))]);
        let foreign = QueryChangeCodec::encode_row(
            "different-query",
            1,
            &variables,
            QueryRowKind::Row,
            RecordImage::Full,
        )
        .unwrap();
        let partial = QueryChangeCodec::encode_row(
            "query",
            1,
            &variables,
            QueryRowKind::Row,
            RecordImage::Partial,
        )
        .unwrap();
        let corrupt = Record::try_new(
            &Schema::new(
                QueryChangeCodec::schema().descriptor().clone(),
                Arc::new(UncheckedRowValidator),
            ),
            partial.identity().clone(),
            RecordImage::Full,
            bytes::Bytes::from_static(b"not a query row"),
        )
        .unwrap();
        for record in [foreign, partial, corrupt] {
            let mut view = snapshot_view(0);
            view.snapshot.rows.insert(record.identity().clone(), record);
            let backend = BootstrapView {
                query: "query".into(),
                view,
                original: None,
                staged: Arc::new(Mutex::new(None)),
            };
            let before = QueryChangeCodec::snapshot_row_projections();
            assert!(matches!(
                backend.fetch_snapshot().await,
                Err(FetchError::NotRunning {
                    status: ComponentStatus::Error
                })
            ));
            assert_eq!(QueryChangeCodec::snapshot_row_projections(), before);
        }
    }

    #[tokio::test]
    async fn empty_snapshot_stream_preserves_metadata_without_projection() {
        let view = snapshot_view(0);
        let before = QueryChangeCodec::snapshot_row_projections();
        let stream = snapshot_stream("query", view.snapshot, view.config_hash).unwrap();
        assert_eq!(stream.as_of_sequence, 10);
        assert_eq!(stream.config_hash, 123);
        assert!(stream.collect_keyed_vec().await.is_empty());
        assert_eq!(QueryChangeCodec::snapshot_row_projections(), before);
    }

    #[test]
    fn outbox_head_lookup_preserves_generation_without_requiring_retained_history() {
        for sequence in [0, 42, u64::MAX] {
            let view = OutputView {
                snapshot: QuerySnapshot {
                    rows: im::HashMap::new(),
                    as_of_sequence: sequence,
                    generation: 7,
                },
                retained: Vec::new(),
                config_hash: 123,
                incarnation: None,
            };
            let head = outbox_response(&view, u64::MAX).unwrap();
            assert!(head.results.is_empty());
            assert_eq!(head.latest_sequence, sequence);
            assert_eq!(head.output_generation, 7);
            assert_eq!(head.config_hash, 123);
            if sequence == 42 {
                assert!(matches!(
                    outbox_response(&view, 0),
                    Err(FetchError::OutboxGap(_))
                ));
                assert!(matches!(
                    outbox_response(&view, 43),
                    Err(FetchError::OutboxGap(_))
                ));
            }
        }
    }
}
