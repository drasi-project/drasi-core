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

//! Graph-owned continuous query host. No legacy query manager, QueryBase, or
//! ComponentGraph is involved in construction, processing or output publication.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
};

use anyhow::Context;
use async_trait::async_trait;
use bincode::Options;
use bytes::Bytes;
use chrono::Utc;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationQuery},
    evaluation::functions::FunctionRegistry,
    interface::{FutureQueue, IndexError, RowMutation, SourceCheckpoint},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_functions_gql::GQLFunctionSet;
use drasi_query_ast::api::QueryParser;
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;
use serde::{Deserialize, Serialize};

use crate::{
    computation::internal::query_state::QueryOutputState,
    profiling::{timestamp_ns, ProfilingMetadata},
};

pub use crate::computation::internal::query_state::{
    QueryHistoryError, QueryRecoveryView, QueryResults, QuerySnapshot,
};

use super::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole,
    ComponentSpecification, ComputationBootstrapProvider, ComputationComponent, ConfigurationField,
    ConfigurationSchema, ConfigurationType, ConfigurationValue, ConstructedComponent,
    ConstructionContext, EnvelopeCodec, FactoryDescriptor, GraphChangeCodec,
    ImplementationIdentity, InputEnvelope, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortDirection, PortId, QueryBootstrapResource, QueryChangeCodec, QueryOptions,
    QueryOutputMetadata, QueryPublicationMode, QueryRecoveryError, QueryRecoveryPolicy, Record,
    RecordId, RecordImage, ResourceRequirement, ResourceRole, StreamId, SystemMetadata,
    Transformer, WakeupSource,
};
use super::{QueryExecutionSettings, QueryMiddlewareResource};
use super::{
    QuerySourceProgress, QuerySourceProgressResource, SourceProgressKey, SourceProgressSnapshot,
};

const CONFIGURATION: &str = "\0computation:query-configuration:v1";
const INPUT_PREFIX: &str = "computation:input:";
const BOOTSTRAP: &str = "\0computation:query-bootstrap:v1";
const PENDING_OUTPUT: &str = "\0computation:pending-output:v1";

pub(crate) fn query_reset_key(query: &str) -> String {
    format!("computation-reset-v1:{query}")
}

#[path = "query_delivery.rs"]
mod delivery;
#[path = "query_recovery.rs"]
mod recovery;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ComputationQueryLanguage {
    #[default]
    Cypher,
    Gql,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuousQueryDefinition {
    pub graph_id: String,
    pub id: ComponentId,
    pub query: String,
    pub language: ComputationQueryLanguage,
    pub output_stream: StreamId,
    pub outbox_capacity: NonZeroUsize,
}

impl ContinuousQueryDefinition {
    pub fn descriptor(&self) -> ComponentDescriptor {
        query_descriptor(self.id.clone())
    }

    fn configuration_bytes(&self, execution: &QueryExecutionSettings) -> anyhow::Result<Bytes> {
        Ok(Bytes::from(serde_json::to_vec(&(
            3u32,
            &self.query,
            self.language,
            self.output_stream.as_str(),
            execution,
        ))?))
    }

    fn configuration_hash(&self, execution: &QueryExecutionSettings) -> anyhow::Result<u64> {
        use std::hash::Hasher;
        let mut hasher = fnv::FnvHasher::default();
        hasher.write(&self.configuration_bytes(execution)?);
        Ok(hasher.finish())
    }
}

fn query_descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![
            PortDescriptor::new(
                PortId::try_new("in").expect("port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ),
            PortDescriptor::new(
                PortId::try_new("out").expect("port"),
                PortDirection::Output,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ),
        ],
    )
    .expect("typed query descriptor")
}

fn parser(language: ComputationQueryLanguage) -> (Arc<dyn QueryParser>, Arc<FunctionRegistry>) {
    match language {
        ComputationQueryLanguage::Cypher => {
            let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
            (Arc::new(CypherParser::new(functions.clone())), functions)
        }
        ComputationQueryLanguage::Gql => {
            let functions = Arc::new(FunctionRegistry::new()).with_gql_function_set();
            (Arc::new(GQLParser::new(functions.clone())), functions)
        }
    }
}

struct FutureWakeup {
    queue: Arc<dyn FutureQueue>,
    keep_alive: bool,
}

#[async_trait]
impl WakeupSource for FutureWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            if let Some(due) = self.queue.peek_due_time().await? {
                let now = u64::try_from(Utc::now().timestamp_millis())
                    .map_err(|_| anyhow::anyhow!("clock before epoch"))?;
                if due <= now {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis((due - now).min(25))).await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.keep_alive || self.queue.peek_due_time().await?.is_some())
    }
}

struct ProcessingGuard {
    failure: Arc<AtomicBool>,
    complete: bool,
    progress: Option<Arc<QuerySourceProgress>>,
}
impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.failure.store(true, Ordering::Release);
            if let Some(progress) = &self.progress {
                progress.fail();
            }
        }
    }
}

struct TransactionTimer {
    metrics: Option<Arc<crate::metrics::QueryOutputMetrics>>,
    started: std::time::Instant,
}

impl Drop for TransactionTimer {
    fn drop(&mut self) {
        if let Some(metrics) = &self.metrics {
            metrics.record_transaction_duration_ns(
                self.started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            );
        }
    }
}

struct InputProgress {
    graph: super::producer_progress::GraphInputProgress,
    source_id: String,
    profiling: Option<ProfilingMetadata>,
}

impl std::ops::Deref for InputProgress {
    type Target = super::producer_progress::GraphInputProgress;
    fn deref(&self) -> &Self::Target {
        &self.graph
    }
}

impl std::ops::DerefMut for InputProgress {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.graph
    }
}

fn input_progress(input: &super::ChangeEnvelope) -> anyhow::Result<InputProgress> {
    let raw = GraphChangeCodec::source_metadata(input)?;
    let source_id = raw
        .as_ref()
        .map(|metadata| metadata.source_id.clone())
        .unwrap_or_else(|| input.system().stream().as_str().to_owned());
    Ok(InputProgress {
        graph: super::producer_progress::GraphInputProgress::from_envelope(input)?,
        source_id,
        profiling: raw.and_then(|metadata| metadata.profiling),
    })
}

pub(crate) fn progress_key(stream: &str, source: Option<&str>) -> String {
    let key = match source {
        Some(source) => format!("source:{source}"),
        None => format!("stream:{stream}"),
    };
    format!(
        "{INPUT_PREFIX}{}",
        key.bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

pub(crate) fn progress_identity(key: &str) -> anyhow::Result<SourceProgressKey> {
    let key = key
        .strip_prefix(INPUT_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("invalid source checkpoint prefix"))?;
    if key.len() % 2 != 0 || !key.is_ascii() {
        anyhow::bail!("invalid source checkpoint identity");
    }
    let decoded = (0..key.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&key[index..index + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    let decoded = String::from_utf8(decoded)?;
    if let Some(source) = decoded.strip_prefix("source:") {
        Ok(SourceProgressKey::Source(source.to_owned()))
    } else if let Some(stream) = decoded.strip_prefix("stream:") {
        Ok(SourceProgressKey::Stream(StreamId::try_new(stream)?))
    } else {
        anyhow::bail!("invalid source checkpoint identity");
    }
}

fn validate_watermarks(watermarks: &[super::BootstrapWatermark]) -> anyhow::Result<()> {
    let mut sources = std::collections::BTreeSet::new();
    for watermark in watermarks {
        if !sources.insert(progress_key(
            watermark.stream.as_str(),
            watermark.source_id.as_deref(),
        )) {
            anyhow::bail!("bootstrap contains duplicate source watermarks");
        }
        if watermark.position.as_ref().is_some_and(|position| {
            position.len() > crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
        }) {
            anyhow::bail!("bootstrap source position exceeds the supported checkpoint limit");
        }
    }
    Ok(())
}

pub struct ContinuousQueryTransformer {
    definition: ContinuousQueryDefinition,
    descriptor: ComponentDescriptor,
    provider: Arc<dyn ComputationIndexProvider>,
    query: Option<ComputationQuery>,
    results: QueryResults,
    codec: EnvelopeCodec,
    failure: Arc<AtomicBool>,
    options: QueryOptions,
    bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
    bootstrap_complete: AtomicBool,
    watermarks: Mutex<HashMap<String, u64>>,
    source_progress: Option<Arc<QuerySourceProgress>>,
    execution: QueryExecutionSettings,
    middleware: Option<Arc<drasi_core::middleware::MiddlewareTypeRegistry>>,
    registration: Option<super::query_catalog::QueryRegistration>,
    metrics: Option<Arc<crate::metrics::QueryOutputMetrics>>,
    reset_configuration: bool,
    runtime_compatibility: bool,
    output_persistent: bool,
    legacy_hash: Option<u64>,
    checkpoint_view: Arc<RwLock<Option<Arc<dyn drasi_core::interface::CheckpointStore>>>>,
    output_persistence_view: Arc<RwLock<Option<bool>>>,
    publication_identity: Arc<RwLock<Option<uuid::Uuid>>>,
    recovery_scope: Arc<str>,
    scheduling: Option<Arc<super::QuerySchedulingResource>>,
    draining_futures: bool,
    delivery_tracking: bool,
    pending_output: VecDeque<super::ChangeEnvelope>,
    replay_pending: Arc<AtomicBool>,
    delivery_changed: Arc<tokio::sync::Notify>,
    delivery_sequence: AtomicU64,
    delivered_output: AtomicU64,
}

impl ContinuousQueryTransformer {
    pub async fn new(
        definition: ContinuousQueryDefinition,
        provider: Arc<dyn ComputationIndexProvider>,
    ) -> anyhow::Result<Self> {
        Self::new_with_options(definition, provider, QueryOptions::default()).await
    }

    pub async fn new_with_options(
        definition: ContinuousQueryDefinition,
        provider: Arc<dyn ComputationIndexProvider>,
        options: QueryOptions,
    ) -> anyhow::Result<Self> {
        Self::new_configured(
            definition,
            provider,
            options,
            QueryExecutionSettings::default(),
            None,
        )
        .await
    }

    pub async fn new_configured(
        definition: ContinuousQueryDefinition,
        provider: Arc<dyn ComputationIndexProvider>,
        options: QueryOptions,
        execution: QueryExecutionSettings,
        middleware: Option<Arc<drasi_core::middleware::MiddlewareTypeRegistry>>,
    ) -> anyhow::Result<Self> {
        Self::construct(definition, provider, options, execution, middleware, false).await
    }
    async fn construct(
        definition: ContinuousQueryDefinition,
        provider: Arc<dyn ComputationIndexProvider>,
        options: QueryOptions,
        execution: QueryExecutionSettings,
        middleware: Option<Arc<drasi_core::middleware::MiddlewareTypeRegistry>>,
        defer_build: bool,
    ) -> anyhow::Result<Self> {
        execution.validate(middleware.as_deref())?;
        if !execution.middleware.is_empty() && middleware.is_none() {
            anyhow::bail!("query middleware requires a registered middleware resource");
        }
        let (parser, _) = parser(definition.language);
        parser.parse(&definition.query)?;
        let mut codec =
            EnvelopeCodec::new(NonZeroUsize::new(64 * 1024 * 1024).expect("storage limit"));
        codec.register_schema(QueryChangeCodec::schema())?;
        let results = QueryResults {
            state: Arc::new(RwLock::new(QueryOutputState::new(
                definition.outbox_capacity,
            ))),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        let mut instance = Self {
            descriptor: definition.descriptor(),
            recovery_scope: Arc::from(definition.graph_id.as_str()),
            definition,
            provider,
            query: None,
            results,
            codec,
            failure: Arc::new(AtomicBool::new(false)),
            options,
            bootstrap: None,
            bootstrap_complete: AtomicBool::new(false),
            watermarks: Mutex::new(HashMap::new()),
            source_progress: None,
            execution,
            middleware,
            registration: None,
            metrics: None,
            reset_configuration: false,
            runtime_compatibility: false,
            output_persistent: false,
            legacy_hash: None,
            checkpoint_view: Arc::new(RwLock::new(None)),
            output_persistence_view: Arc::new(RwLock::new(None)),
            publication_identity: Arc::new(RwLock::new(Some(uuid::Uuid::new_v4()))),
            scheduling: None,
            draining_futures: false,
            delivery_tracking: false,
            pending_output: VecDeque::new(),
            replay_pending: Arc::new(AtomicBool::new(false)),
            delivery_changed: Arc::new(tokio::sync::Notify::new()),
            delivery_sequence: AtomicU64::new(0),
            delivered_output: AtomicU64::new(0),
        };
        if !defer_build {
            instance.build().await?;
        }
        Ok(instance)
    }

    fn result_metadata(
        &self,
        source_id: &str,
        results: &[drasi_core::evaluation::context::QueryPartEvaluationContext],
    ) -> HashMap<String, serde_json::Value> {
        if !self.runtime_compatibility {
            return HashMap::new();
        }
        let count = results
            .iter()
            .filter(|result| {
                !matches!(
                    result,
                    drasi_core::evaluation::context::QueryPartEvaluationContext::Noop
                )
            })
            .count();
        HashMap::from([
            ("source_id".into(), serde_json::json!(source_id)),
            ("processed_by".into(), serde_json::json!("drasi-core")),
            ("result_count".into(), serde_json::json!(count)),
        ])
    }

    pub fn results(&self) -> QueryResults {
        self.results.clone()
    }

    /// Bind identity to an enclosing instance/storage scope before registration
    /// or activation. Factories supply their construction scope automatically;
    /// standalone queries otherwise use their definition's graph scope.
    pub fn with_recovery_scope(mut self, scope: impl Into<Arc<str>>) -> anyhow::Result<Self> {
        let scope = scope.into();
        super::data::validate_identifier("query construction scope", &scope)?;
        {
            let state = self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            if self.registration.is_some()
                || state.recovery_ready
                || state.sequence != 0
                || !state.rows.is_empty()
                || self.failure.load(Ordering::Acquire)
            {
                anyhow::bail!(
                    "query recovery scope must be selected before registration or activation"
                );
            }
        }
        self.recovery_scope = scope;
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .identity = Some(self.recovery_identity()?);
        Ok(self)
    }

    pub fn with_result_catalog(
        mut self,
        catalog: &super::QueryResultsCatalog,
    ) -> anyhow::Result<Self> {
        if catalog.graph_id() != self.definition.graph_id {
            anyhow::bail!("query catalogue belongs to another graph");
        }
        self.metrics = catalog.configured_metrics(&self.definition.id)?;
        self.legacy_hash = catalog.configured_hash(&self.definition.id)?;
        self.registration = Some(catalog.register(
            self.definition.id.clone(),
            self.results.clone(),
            self.source_progress.clone(),
            self.definition.configuration_hash(&self.execution)?,
            self.publication_identity.clone(),
            self.checkpoint_view.clone(),
            self.output_persistence_view.clone(),
        )?);
        Ok(self)
    }

    pub fn with_bootstrap(mut self, provider: Arc<dyn ComputationBootstrapProvider>) -> Self {
        self.bootstrap = Some(provider);
        self
    }

    pub fn with_scheduling(
        mut self,
        scheduling: Arc<super::QuerySchedulingResource>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query state poisoned"))?
                .ready,
            "bind the scheduling resource before starting a query"
        );
        self.scheduling = Some(scheduling);
        Ok(self)
    }

    pub fn with_source_progress(
        mut self,
        progress: Arc<QuerySourceProgress>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !progress.replay_only(),
            "query cannot own middleware's replay-only source progress"
        );
        if progress.graph_id() != self.definition.graph_id
            || progress.query_id() != &self.definition.id
        {
            anyhow::bail!("source progress belongs to another graph/query");
        }
        self.source_progress = Some(progress);
        Ok(self)
    }

    async fn publish_source_progress(&self, ready: bool) -> anyhow::Result<()> {
        let Some(progress) = &self.source_progress else {
            return Ok(());
        };
        let mut checkpoints = BTreeMap::new();
        let store = self.query()?.resources().checkpoint_store();
        let persistent =
            store.is_some_and(|store| store.is_persistent()) && !self.provider.is_volatile();
        if let Some(store) = store {
            for (key, checkpoint) in store.read_all_checkpoints().await? {
                if key.starts_with(INPUT_PREFIX) {
                    checkpoints.insert(progress_identity(&key)?, checkpoint);
                }
            }
        } else {
            let previous = progress.snapshot();
            for (key, sequence) in self
                .watermarks
                .lock()
                .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
                .iter()
            {
                let key = progress_identity(key)?;
                let position = previous
                    .checkpoints
                    .get(&key)
                    .filter(|saved| saved.sequence == *sequence)
                    .and_then(|saved| saved.source_position.clone());
                checkpoints.insert(key, SourceCheckpoint::new(*sequence, position));
            }
        }
        progress.publish(SourceProgressSnapshot {
            ready,
            admitting: ready,
            recovered: true,
            bootstrap_complete: self.bootstrap_complete.load(Ordering::Acquire),
            persistent,
            reset_generation: self.results.snapshot()?.generation,
            checkpoints,
            failure: None,
        });
        Ok(())
    }

    async fn build(&mut self) -> anyhow::Result<()> {
        *self
            .output_persistence_view
            .write()
            .map_err(|_| anyhow::anyhow!("query output persistence view poisoned"))? = None;
        let mut resources = self
            .provider
            .create_indexes(&self.definition.graph_id, self.definition.id.as_str())
            .await?;
        if self.runtime_compatibility {
            resources = resources.with_fallback_checkpoint(Arc::new(drasi_core::in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore::new()));
        }
        self.output_persistent = !self.provider.is_volatile()
            && resources
                .checkpoint_store()
                .is_some_and(|store| store.is_persistent())
            && resources.outbox_writer().is_some()
            && resources.live_results_writer().is_some();
        {
            let mut identity = self
                .publication_identity
                .write()
                .map_err(|_| anyhow::anyhow!("query output identity poisoned"))?;
            if self.output_persistent {
                *identity = None;
            } else if identity.is_none() {
                *identity = Some(uuid::Uuid::new_v4());
            }
        }
        if !self.provider.is_volatile() && self.options.publication == QueryPublicationMode::Atomic
        {
            resources.atomic_result_transaction()?;
        }
        if !self.runtime_compatibility && !self.provider.is_volatile() && !self.output_persistent {
            anyhow::bail!(
                "persistent query recovery requires persistent checkpoint, outbox and live-result resources"
            );
        }
        let (parser, functions) = parser(self.definition.language);
        let builder = self.execution.configure(
            QueryBuilder::new(&self.definition.query, parser).with_function_registry(functions),
            self.middleware.clone(),
        )?;
        *self
            .checkpoint_view
            .write()
            .map_err(|_| anyhow::anyhow!("query checkpoint view poisoned"))? =
            resources.checkpoint_store().cloned();
        self.query = Some(ComputationQuery::try_build(builder, resources).await?);
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .identity = Some(self.recovery_identity()?);
        *self
            .output_persistence_view
            .write()
            .map_err(|_| anyhow::anyhow!("query output persistence view poisoned"))? =
            Some(self.output_persistent);
        Ok(())
    }

    fn query(&self) -> anyhow::Result<&ComputationQuery> {
        self.query
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query requires reconstruction"))
    }

    fn recovery_identity(&self) -> anyhow::Result<super::QueryRecoveryIdentity> {
        Ok(super::QueryRecoveryIdentity::try_new(
            self.definition.graph_id.clone(),
            self.definition.id.clone(),
            self.definition.configuration_hash(&self.execution)?,
            *self
                .publication_identity
                .read()
                .map_err(|_| anyhow::anyhow!("query output identity poisoned"))?,
        )?
        .with_construction_scope(&self.recovery_scope)?)
    }

    fn stamp_output_identity(&self, output: &mut super::ChangeEnvelope) -> anyhow::Result<()> {
        let state = self
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        let identity = state
            .identity
            .as_ref()
            .ok_or(QueryHistoryError::MissingIdentity)?;
        QueryChangeCodec::set_generation(output, &self.definition.id, state.generation)?;
        identity.annotate(output, &self.definition.id)?;
        Ok(())
    }

    async fn recover(&self) -> anyhow::Result<()> {
        let query = self.query()?;
        let Some(checkpoint) = query.resources().checkpoint_store() else {
            return Ok(());
        };
        if self.runtime_compatibility && checkpoint.is_persistent() {
            if let Some(expected) = self.legacy_hash {
                match checkpoint.read_config_hash().await {
                    Ok(Some(actual)) if actual == expected => {}
                    Ok(_) => return Err(QueryRecoveryError::ConfigurationChanged.into()),
                    Err(error) => {
                        log::warn!("Query {} cannot read its configuration hash; rebootstrap is required: {error}", self.definition.id);
                        return Err(anyhow::Error::new(QueryRecoveryError::ConfigurationChanged)
                            .context(error.to_string()));
                    }
                }
            }
        }
        if let Some(marker) = checkpoint.read_checkpoint(BOOTSTRAP).await? {
            if marker.sequence > 1 || marker.source_position.is_some() {
                return Err(
                    QueryRecoveryError::Inconsistent("invalid bootstrap marker".into()).into(),
                );
            }
            if marker.sequence == 0 {
                return Err(QueryRecoveryError::IncompleteBootstrap.into());
            }
            self.bootstrap_complete.store(true, Ordering::Release);
        }
        if checkpoint
            .read_checkpoint(PENDING_OUTPUT)
            .await?
            .is_some_and(|marker| marker.sequence > 0 || marker.source_position.is_some())
        {
            return Err(QueryRecoveryError::PendingPublication.into());
        }
        let reset = self.read_reset_marker().await?;
        if reset.as_ref().is_some_and(|marker| marker.in_progress) {
            return Err(QueryRecoveryError::IncompleteReset.into());
        }
        let generation = checkpoint
            .read_output_generation(self.definition.id.as_str())
            .await?
            .unwrap_or(0)
            .max(reset.as_ref().map_or(0, |marker| marker.generation));
        let configuration = self.definition.configuration_bytes(&self.execution)?;
        let stored = checkpoint.read_checkpoint(CONFIGURATION).await?;
        let configuration_missing = stored.is_none();
        let configuration_verified = stored.as_ref().is_some_and(|stored| {
            stored.sequence == 1 && stored.source_position.as_ref() == Some(&configuration)
        });
        if let Some(stored) = stored {
            if stored.sequence != 1 {
                return Err(QueryRecoveryError::Inconsistent(
                    "unsupported query configuration record".into(),
                )
                .into());
            }
            if stored.source_position.as_ref() != Some(&configuration) {
                return Err(QueryRecoveryError::ConfigurationChanged.into());
            }
        }
        if self.runtime_compatibility && !self.output_persistent {
            if configuration_missing {
                self.write_configuration(&configuration).await?;
            }
            *self
                .watermarks
                .lock()
                .map_err(|_| anyhow::anyhow!("watermarks poisoned"))? = checkpoint
                .read_all_checkpoints()
                .await?
                .into_iter()
                .filter(|(key, _)| key.starts_with(INPUT_PREFIX))
                .map(|(key, checkpoint)| (key, checkpoint.sequence))
                .collect();
            return Ok(());
        }
        let sequence = checkpoint
            .read_result_sequence(self.definition.id.as_str())
            .await?;
        let outbox = query
            .resources()
            .outbox_writer()
            .ok_or_else(|| anyhow::anyhow!("missing output store"))?
            .read_from(self.definition.id.as_str(), 0)
            .await?;
        let live = query
            .resources()
            .live_results_writer()
            .ok_or_else(|| anyhow::anyhow!("missing live-result store"))?
            .read_snapshot(self.definition.id.as_str())
            .await?;
        if sequence.is_none() && (!outbox.is_empty() || !live.is_empty()) {
            return Err(QueryRecoveryError::Inconsistent(
                "state exists without a committed sequence".into(),
            )
            .into());
        }
        let sequence = sequence.unwrap_or(0);
        let baseline = outbox.is_empty()
            && reset
                .as_ref()
                .is_some_and(|marker| !marker.in_progress && marker.sequence == sequence);
        if sequence > 0
            && !baseline
            && outbox.last().map(|(sequence, _)| *sequence) != Some(sequence)
        {
            return Err(QueryRecoveryError::Inconsistent(
                "retained tail differs from committed sequence".into(),
            )
            .into());
        }
        let mut rows = im::HashMap::new();
        for (signature, bytes) in live {
            let namespace = format!(
                "drasi.query-row/{}",
                self.definition
                    .id
                    .as_str()
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let row = Record::try_new(
                &QueryChangeCodec::schema(),
                RecordId::try_new(namespace, Bytes::copy_from_slice(&signature.to_be_bytes()))?,
                RecordImage::Full,
                Bytes::from(bytes),
            )
            .with_context(|| {
                QueryRecoveryError::Inconsistent(format!(
                    "failed to deserialize durable live row {signature}"
                ))
            })?;
            rows.insert(row.identity().clone(), row);
        }
        let mut recovered = Vec::new();
        let identity = self.recovery_identity()?;
        let mut previous = None;
        let mut expected = HashMap::new();
        for (position, bytes) in outbox {
            if previous.is_some_and(|previous: u64| previous.checked_add(1) != Some(position)) {
                return Err(QueryRecoveryError::Inconsistent(
                    "retained history has an interior gap".into(),
                )
                .into());
            }
            let mut envelope = self.codec.decode(&bytes).with_context(|| {
                QueryRecoveryError::Inconsistent(format!(
                    "failed to deserialize durable outbox entry {position}"
                ))
            })?;
            let metadata = QueryChangeCodec::metadata(&envelope).with_context(|| {
                QueryRecoveryError::Inconsistent(format!(
                    "invalid durable outbox metadata at {position}"
                ))
            })?;
            if envelope.system().sequence() != position
                || envelope.system().stream() != &self.definition.output_stream
                || metadata.query_id != self.definition.id.as_str()
            {
                return Err(QueryRecoveryError::Inconsistent(
                    "retained output identity differs from the query".into(),
                )
                .into());
            }
            if QueryChangeCodec::query_generation(&envelope).with_context(|| {
                QueryRecoveryError::Inconsistent(format!(
                    "invalid durable outbox generation at {position}"
                ))
            })? != generation
            {
                return Err(QueryRecoveryError::Inconsistent(
                    "retained output belongs to another reset generation".into(),
                )
                .into());
            }
            match super::QueryRecoveryIdentity::optional_from_envelope(&envelope).with_context(
                || {
                    QueryRecoveryError::Inconsistent(format!(
                        "invalid durable outbox producer identity at {position}"
                    ))
                },
            )? {
                Some(stored) if stored == identity => {}
                Some(_) => {
                    return Err(QueryRecoveryError::Inconsistent(
                        "retained output belongs to another producer identity".into(),
                    )
                    .into())
                }
                None if configuration_verified => {
                    identity.annotate(&mut envelope, &self.definition.id)?;
                }
                None => {
                    return Err(QueryRecoveryError::Inconsistent(
                        "unidentified retained output has no verified owner configuration".into(),
                    )
                    .into())
                }
            }
            for operation in envelope.changes().operations() {
                match operation {
                    super::ChangeOperation::Added { after, .. }
                    | super::ChangeOperation::Updated { after, .. } => {
                        expected.insert(after.identity().clone(), Some(after.payload().clone()));
                    }
                    super::ChangeOperation::Deleted { identity, .. } => {
                        expected.insert(identity.identity().clone(), None);
                    }
                }
            }
            previous = Some(position);
            recovered.push(envelope);
        }
        for (id, expected) in expected {
            if rows.get(&id).map(|row| row.payload()) != expected.as_ref() {
                return Err(QueryRecoveryError::Inconsistent(
                    "snapshot contradicts retained history".into(),
                )
                .into());
            }
        }
        if configuration_missing {
            // A failed recovery must not manufacture the owner evidence that
            // would let the next start adopt unidentified retained output.
            self.write_configuration(&configuration).await?;
        }
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .hydrate(rows, sequence, recovered, generation, identity);
        let saved = checkpoint.read_all_checkpoints().await?;
        *self
            .watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))? = saved
            .into_iter()
            .filter(|(key, _)| key.starts_with(INPUT_PREFIX))
            .map(|(key, checkpoint)| (key, checkpoint.sequence))
            .collect();
        Ok(())
    }

    async fn write_configuration(&self, configuration: &Bytes) -> anyhow::Result<()> {
        let query = self.query()?;
        let checkpoint = query
            .resources()
            .checkpoint_store()
            .ok_or_else(|| anyhow::anyhow!("missing query configuration store"))?;
        query
            .resource_transaction(|| async {
                checkpoint
                    .stage_checkpoint(CONFIGURATION, 1, Some(configuration))
                    .await
            })
            .await?;
        Ok(())
    }

    fn next_sequence(&self) -> anyhow::Result<u64> {
        self.results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .next_sequence()
    }

    async fn stage_output(&self, output: &super::ChangeEnvelope) -> Result<(), IndexError> {
        let query = self
            .query()
            .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        let bytes = self.codec.encode(output).map_err(IndexError::other)?;
        let id = self.definition.id.as_str();
        let resources = query.resources();
        let sequence = output.system().sequence();
        let retain_from = sequence
            .saturating_sub(self.definition.outbox_capacity.get() as u64)
            .saturating_add(1);
        if self.delivery_tracking
            && retain_from.saturating_sub(1) > self.delivered_output.load(Ordering::Acquire)
        {
            return Err(IndexError::Other(
                anyhow::anyhow!("query output retention cannot evict unconfirmed handoff")
                    .into_boxed_dyn_error(),
            ));
        }
        resources
            .outbox_writer()
            .ok_or(IndexError::NotSupported)?
            .append_and_trim(id, sequence, &bytes, retain_from)
            .await?;
        self.stage_projection(output).await?;
        resources
            .checkpoint_store()
            .ok_or(IndexError::NotSupported)?
            .stage_result_sequence(id, sequence)
            .await?;
        self.stage_delivery().await
    }

    async fn stage_projection(&self, output: &super::ChangeEnvelope) -> Result<(), IndexError> {
        let query = self
            .query()
            .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        let mutations = output
            .changes()
            .operations()
            .iter()
            .map(|operation| {
                let signature = u64::from_be_bytes(
                    operation
                        .identity()
                        .value()
                        .as_ref()
                        .try_into()
                        .map_err(|_| IndexError::CorruptedData)?,
                );
                let data = match operation {
                    super::ChangeOperation::Added { after, .. }
                    | super::ChangeOperation::Updated { after, .. } => {
                        Some(after.payload().as_ref())
                    }
                    super::ChangeOperation::Deleted { .. } => None,
                };
                Ok(RowMutation {
                    row_signature: signature,
                    data,
                })
            })
            .collect::<Result<Vec<_>, IndexError>>()?;
        query
            .resources()
            .live_results_writer()
            .ok_or(IndexError::NotSupported)?
            .apply_mutations(self.definition.id.as_str(), &mutations)
            .await
    }

    fn publish(
        &self,
        output: Option<super::ChangeEnvelope>,
    ) -> anyhow::Result<Vec<OutputEnvelope>> {
        match output {
            Some(output) => {
                self.results
                    .state
                    .write()
                    .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
                    .apply(output.clone())?;
                self.results.notify.notify_waiters();
                if let Some(metrics) = &self.metrics {
                    metrics.record_seq_advance();
                }
                self.sync_metrics()?;
                Ok(vec![OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: self.live_delivery(output)?,
                }])
            }
            None => Ok(Vec::new()),
        }
    }
    fn sync_metrics(&self) -> anyhow::Result<()> {
        if let Some(metrics) = &self.metrics {
            let state = self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            metrics.update_outbox(
                state.outbox.len(),
                state
                    .outbox
                    .front()
                    .map_or(0, |output| output.system().sequence()),
                state.sequence,
            );
            metrics.record_live_results_count(state.rows.len());
        }
        Ok(())
    }

    async fn process(&self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        if self.failure.load(Ordering::Acquire) {
            anyhow::bail!("query processing is fenced pending recovery");
        }
        let query = self.query()?;
        let mut progress = input_progress(&input.envelope)?;
        let saved = if let Some(checkpoint) = query.resources().checkpoint_store() {
            progress
                .graph
                .validate(
                    checkpoint.as_ref(),
                    !self.provider.is_volatile() && checkpoint.is_persistent(),
                )
                .await?
        } else {
            None
        };
        if self
            .watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
            .get(&progress.key)
            .is_some_and(|saved| *saved >= progress.sequence)
        {
            return Ok(Vec::new());
        }
        if query.resources().checkpoint_store().is_some() {
            if saved
                .as_ref()
                .is_some_and(|saved| saved.sequence >= progress.sequence)
            {
                return Ok(Vec::new());
            }
            if progress.position.as_ref().is_some_and(|position| {
                position.len() > crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
            }) {
                log::warn!("Query {} retains its last valid checkpoint cursor because the new source position is oversized", self.definition.id);
                progress.position = saved.and_then(|saved| saved.source_position);
            }
        }
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        self.begin_non_atomic().await?;
        let prepared = Mutex::new(None);
        let mut profiling = progress.profiling.clone().unwrap_or_else(|| {
            if self.runtime_compatibility {
                ProfilingMetadata::new()
            } else {
                ProfilingMetadata::default()
            }
        });
        // An input may carry another query's timings; this commit has not completed.
        profiling.query_core_return_ns = None;
        profiling.query_send_ns = None;
        profiling.query_receive_ns = Some(timestamp_ns());
        profiling.query_core_call_ns = Some(timestamp_ns());
        let hook = |results: Arc<[drasi_core::evaluation::context::QueryPartEvaluationContext]>| {
            let prepared = &prepared;
            let input = &input;
            let progress = &progress;
            let profiling = profiling.clone();
            async move {
                let sequence = if results.iter().any(|result| {
                    !matches!(
                        result,
                        drasi_core::evaluation::context::QueryPartEvaluationContext::Noop
                    )
                }) {
                    self.next_sequence()
                        .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?
                } else {
                    0
                };
                let mut output = QueryChangeCodec::encode_evaluation(
                    Some(&input.envelope),
                    &self.definition.id,
                    SystemMetadata::new(self.definition.output_stream.clone(), sequence)
                        .with_timestamp(
                            input.envelope.system().timestamp().unwrap_or_else(Utc::now),
                        ),
                    &results,
                    QueryOutputMetadata {
                        query_id: self.definition.id.to_string(),
                        source_id: Some(progress.source_id.clone()),
                        timestamp: Utc::now(),
                        metadata: self.result_metadata(&progress.source_id, &results),
                        profiling: Some(profiling),
                    },
                )
                .map_err(IndexError::other)?;
                if let Some(output) = &mut output {
                    self.stamp_output_identity(output)
                        .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
                }
                if let Some(checkpoint) = query.resources().checkpoint_store() {
                    progress.graph.stage(checkpoint.as_ref()).await?;
                    if let Some(output) = &output {
                        if self.output_persistent {
                            if self.options.publication == QueryPublicationMode::Atomic {
                                self.stage_output(output).await?;
                            } else {
                                checkpoint
                                    .stage_checkpoint(
                                        PENDING_OUTPUT,
                                        output.system().sequence(),
                                        None,
                                    )
                                    .await?;
                            }
                        }
                    }
                }
                *prepared.lock().map_err(|_| IndexError::CorruptedData)? = output;
                Ok(())
            }
        };
        match query.resources().atomic_result_transaction() {
            Ok(transaction) if self.options.publication == QueryPublicationMode::Atomic => {
                query
                    .process_source_changes_with_result_hook(changes, &transaction, hook)
                    .await?;
            }
            _ if self.provider.is_volatile()
                || self.options.publication == QueryPublicationMode::NonAtomic =>
            {
                query
                    .process_source_changes_with_non_atomic_result_hook(changes, hook)
                    .await?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => unreachable!(),
        }
        let core_return_ns = timestamp_ns();
        let mut output = prepared
            .into_inner()
            .map_err(|_| anyhow::anyhow!("prepared output poisoned"))?;
        if self.options.publication == QueryPublicationMode::NonAtomic && self.output_persistent {
            self.finish_non_atomic(output.as_ref()).await?;
        }
        self.watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
            .insert(progress.graph.key.clone(), progress.sequence);
        if let Some(confirmation) = &self.source_progress {
            confirmation.confirm(
                progress.graph.identity.clone(),
                SourceCheckpoint::new(progress.sequence, progress.graph.position),
            );
        }
        // Keep completion timings on the live branch, not in already committed outbox bytes.
        if let Some(output) = &mut output {
            QueryChangeCodec::append_post_commit_profiling(
                output,
                &self.definition.id,
                core_return_ns,
                timestamp_ns(),
            )?;
        }
        self.publish(output)
    }
}

impl Drop for ContinuousQueryTransformer {
    fn drop(&mut self) {
        if let Some(scheduling) = &self.scheduling {
            scheduling.stopped();
        }
        *self.checkpoint_view.write().unwrap_or_else(|error| {
            log::error!("Releasing a poisoned query checkpoint view: {error}");
            error.into_inner()
        }) = None;
    }
}

#[async_trait]
impl ComputationComponent for ContinuousQueryTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "query": self.definition.query,
            "language": match self.definition.language {
                ComputationQueryLanguage::Cypher => "cypher",
                ComputationQueryLanguage::Gql => "gql",
            },
            "stream": self.definition.output_stream,
            "outbox_capacity": self.definition.outbox_capacity,
            "execution": self.execution,
            "recovery": match self.options.recovery {
                QueryRecoveryPolicy::Strict => "strict",
                QueryRecoveryPolicy::AutoReset => "auto_reset",
            },
            "publication": match self.options.publication {
                QueryPublicationMode::Atomic => "atomic",
                QueryPublicationMode::NonAtomic => "non_atomic",
            },
        }))
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        {
            let mut state = self
                .results
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            state.ready = false;
            state.recovery_ready = false;
        }
        if let Some(scheduling) = &self.scheduling {
            scheduling.stopped();
        }
        if let Some(progress) = &self.source_progress {
            progress.pending();
        }
        let mut guard = ProcessingGuard {
            failure: self.failure.clone(),
            complete: false,
            progress: self.source_progress.clone(),
        };
        if self.failure.load(Ordering::Acquire)
            && self.provider.is_volatile()
            && (self.options.recovery != QueryRecoveryPolicy::AutoReset || self.bootstrap.is_none())
        {
            anyhow::bail!("failed volatile query requires explicit reconstruction and bootstrap");
        }
        if self.query.is_none() {
            self.build().await?;
        }
        self.initialize_recovery().await?;
        self.prepare_delivery().await?;
        self.publish_source_progress(true).await?;
        guard.complete = true;
        self.failure.store(false, Ordering::Release);
        {
            let mut state = self
                .results
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            state.ready = true;
            state.recovery_ready = true;
        }
        self.results.notify.notify_waiters();
        self.sync_metrics()?;
        if let Some(scheduling) = &self.scheduling {
            scheduling.ready(self.query()?.scheduling_queue());
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.draining_futures = false;
        self.replay_pending.store(false, Ordering::Release);
        if let Some(scheduling) = &self.scheduling {
            scheduling.stopped();
        }
        if let Some(progress) = &self.source_progress {
            progress.pending();
        }
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .ready = false;
        if self.failure.load(Ordering::Acquire)
            || self
                .query
                .as_ref()
                .is_some_and(ComputationQuery::recovery_required)
        {
            if let Some(query) = &self.query {
                query.shutdown().await?;
            }
            self.query = None;
        } else if let Some(query) = &self.query {
            query.quiesce().await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Transformer for ContinuousQueryTransformer {
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.confirm_delivery(outputs).await
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.pending_output.is_empty(),
            "drain recovered query output before accepting input"
        );
        let futures_due = GraphChangeCodec::is_futures_due(&input.envelope);
        let progress = if futures_due {
            super::GraphProducerProgress::from_envelope(&input.envelope)?
        } else {
            None
        };
        if futures_due
            && progress.as_ref().map_or(true, |progress| {
                !progress.identity().persistent() && self.provider.is_volatile()
            })
        {
            self.draining_futures = true;
            return self.on_wakeup().await;
        }
        let _timer = TransactionTimer {
            metrics: self.metrics.clone(),
            started: std::time::Instant::now(),
        };
        let mut guard = ProcessingGuard {
            failure: self.failure.clone(),
            complete: false,
            progress: self.source_progress.clone(),
        };
        let mut result = self.process(input).await;
        if futures_due && result.is_ok() {
            // The empty batch commits the middleware's logical input progress.
            // Due work already belongs to the query's durable future queue, so
            // an interruption between this checkpoint and draining is replayable.
            self.draining_futures = true;
            result = self.on_wakeup().await;
        }
        guard.complete = result.is_ok();
        result
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        if self.delivery_tracking {
            return self.query.as_ref().map(|query| {
                Arc::new(delivery::QueryDeliveryWakeup {
                    scheduled: self.scheduling.is_none().then(|| FutureWakeup {
                        queue: query.future_queue(),
                        keep_alive: self.runtime_compatibility,
                    }),
                    pending: self.replay_pending.clone(),
                    changed: self.delivery_changed.clone(),
                }) as Arc<dyn WakeupSource>
            });
        }
        if self.scheduling.is_some() {
            return None;
        }
        self.query.as_ref().map(|query| {
            Arc::new(FutureWakeup {
                queue: query.future_queue(),
                keep_alive: self.runtime_compatibility,
            }) as Arc<dyn WakeupSource>
        })
    }

    fn has_pending_emissions(&self) -> bool {
        self.draining_futures || !self.pending_output.is_empty()
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.on_wakeup().await
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.pending_output.is_empty() {
            return self.replay_output().await;
        }
        let _timer = TransactionTimer {
            metrics: self.metrics.clone(),
            started: std::time::Instant::now(),
        };
        if self.failure.load(Ordering::Acquire) {
            anyhow::bail!("query processing is fenced pending recovery");
        }
        let mut guard = ProcessingGuard {
            failure: self.failure.clone(),
            complete: false,
            progress: self.source_progress.clone(),
        };
        let now = u64::try_from(Utc::now().timestamp_millis())
            .map_err(|_| anyhow::anyhow!("clock before epoch"))?;
        if self
            .query()?
            .future_queue()
            .peek_due_time()
            .await?
            .map_or(true, |due| due > now)
        {
            self.draining_futures = false;
            guard.complete = true;
            return Ok(Vec::new());
        }
        let query = self.query()?;
        self.begin_non_atomic().await?;
        let prepared = Mutex::new(None);
        let owner = &*self;
        let hook = |due: Arc<drasi_core::computation::ComputationFutureResult>| {
            let prepared = &prepared;
            async move {
                let sequence = if due.results.iter().any(|result| {
                    !matches!(
                        result,
                        drasi_core::evaluation::context::QueryPartEvaluationContext::Noop
                    )
                }) {
                    owner
                        .next_sequence()
                        .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?
                } else {
                    0
                };
                let timestamp = chrono::DateTime::from_timestamp_millis(
                    i64::try_from(due.future.due_time).map_err(IndexError::other)?,
                )
                .ok_or(IndexError::CorruptedData)?;
                let trigger = GraphChangeCodec::encode_change(
                    drasi_core::models::SourceChange::Future {
                        future_ref: due.future.clone(),
                    },
                    StreamId::try_new(format!("{}/future", owner.definition.id))
                        .map_err(IndexError::other)?,
                    sequence,
                    Some(timestamp),
                )
                .map_err(IndexError::other)?;
                let mut output = QueryChangeCodec::encode_evaluation(
                    Some(&trigger),
                    &owner.definition.id,
                    SystemMetadata::new(owner.definition.output_stream.clone(), sequence)
                        .with_timestamp(timestamp),
                    &due.results,
                    QueryOutputMetadata {
                        query_id: owner.definition.id.to_string(),
                        source_id: Some(due.source_id.to_string()),
                        timestamp: if owner.runtime_compatibility {
                            Utc::now()
                        } else {
                            timestamp
                        },
                        metadata: owner.result_metadata(&due.source_id, &due.results),
                        profiling: owner.runtime_compatibility.then(ProfilingMetadata::new),
                    },
                )
                .map_err(IndexError::other)?;
                if let Some(output) = &mut output {
                    owner
                        .stamp_output_identity(output)
                        .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
                }
                if owner.output_persistent {
                    if let Some(output) = &output {
                        if owner.options.publication == QueryPublicationMode::Atomic {
                            owner.stage_output(output).await?;
                        } else {
                            query
                                .resources()
                                .checkpoint_store()
                                .ok_or(IndexError::NotSupported)?
                                .stage_checkpoint(PENDING_OUTPUT, output.system().sequence(), None)
                                .await?;
                        }
                    }
                }
                *prepared.lock().map_err(|_| IndexError::CorruptedData)? = output;
                Ok(())
            }
        };
        match query.resources().atomic_result_transaction() {
            Ok(transaction) if self.options.publication == QueryPublicationMode::Atomic => {
                query
                    .process_due_futures_with_result_hook(&transaction, hook)
                    .await?;
            }
            _ if self.provider.is_volatile()
                || self.options.publication == QueryPublicationMode::NonAtomic =>
            {
                query
                    .process_due_futures_with_non_atomic_result_hook(hook)
                    .await?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => unreachable!(),
        }
        let output = prepared
            .into_inner()
            .map_err(|_| anyhow::anyhow!("prepared future output poisoned"))?;
        if self.options.publication == QueryPublicationMode::NonAtomic && self.output_persistent {
            self.finish_non_atomic(output.as_ref()).await?;
        }
        let result = self.publish(output);
        guard.complete = result.is_ok();
        result
    }
}

pub struct QueryIndexProviderResource(pub Arc<dyn ComputationIndexProvider>);

pub struct ContinuousQueryFactory {
    descriptor: FactoryDescriptor,
}

impl Default for ContinuousQueryFactory {
    fn default() -> Self {
        let fields = [
            ("query", ConfigurationType::String, true),
            ("stream", ConfigurationType::String, true),
            ("language", ConfigurationType::String, false),
            ("outbox_capacity", ConfigurationType::Integer, false),
            ("recovery", ConfigurationType::String, false),
            ("publication", ConfigurationType::String, false),
            ("execution", ConfigurationType::Object, false),
            ("defer_build", ConfigurationType::Boolean, false),
            ("reset_configuration", ConfigurationType::Boolean, false),
            ("runtime_compatibility", ConfigurationType::Boolean, false),
        ]
        .into_iter()
        .map(|(name, value_type, required)| {
            (
                Arc::from(name),
                ConfigurationField {
                    value_type,
                    required,
                    secret: false,
                },
            )
        })
        .collect();
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/continuous-query", "1")
                    .expect("implementation"),
                role: ComponentRole::Query,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields,
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([
                    (
                        Arc::from("scheduling"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<super::QuerySchedulingResource>(
                                ResourceRole::FutureQueue,
                            )
                        },
                    ),
                    (
                        Arc::from("catalog"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<super::QueryResultsCatalog>(
                                ResourceRole::QueryCatalog,
                            )
                        },
                    ),
                    (
                        Arc::from("middleware"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QueryMiddlewareResource>(
                                ResourceRole::Middleware,
                            )
                        },
                    ),
                    (
                        Arc::from("indexes"),
                        ResourceRequirement::exactly_one::<QueryIndexProviderResource>(
                            ResourceRole::IndexBackend,
                        ),
                    ),
                    (
                        Arc::from("bootstrap"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QueryBootstrapResource>(
                                ResourceRole::Bootstrap,
                            )
                        },
                    ),
                    (
                        Arc::from("source_progress"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                                ResourceRole::Checkpoint,
                            )
                        },
                    ),
                ]),
            },
        }
    }
}

fn language(value: Option<&serde_json::Value>) -> anyhow::Result<ComputationQueryLanguage> {
    match value
        .and_then(serde_json::Value::as_str)
        .unwrap_or("cypher")
    {
        "cypher" => Ok(ComputationQueryLanguage::Cypher),
        "gql" => Ok(ComputationQueryLanguage::Gql),
        _ => anyhow::bail!("unsupported computation query language"),
    }
}

fn options(config: &BTreeMap<Arc<str>, serde_json::Value>) -> anyhow::Result<QueryOptions> {
    let recovery = match config
        .get("recovery")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("strict")
    {
        "strict" => QueryRecoveryPolicy::Strict,
        "auto_reset" => QueryRecoveryPolicy::AutoReset,
        _ => anyhow::bail!("unsupported query recovery policy"),
    };
    let publication = match config
        .get("publication")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("atomic")
    {
        "atomic" => QueryPublicationMode::Atomic,
        "non_atomic" => QueryPublicationMode::NonAtomic,
        _ => anyhow::bail!("unsupported query publication mode"),
    };
    Ok(QueryOptions {
        recovery,
        publication,
    })
}

#[async_trait]
impl ComponentFactory for ContinuousQueryFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate_scope(
        &self,
        graph_id: &str,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<super::ResourceId, super::ResourceSpecification>,
        resources: &BTreeMap<super::ResourceId, super::ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate_resources(spec, declarations, resources)?;
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("execution") {
            let execution: QueryExecutionSettings = serde_json::from_value(value.clone())?;
            let registry = spec
                .dependencies
                .get("middleware")
                .into_iter()
                .flatten()
                .find_map(|id| resources.get(id))
                .map(|handle| {
                    handle
                        .get::<QueryMiddlewareResource>()
                        .map(|resource| resource.0.clone())
                })
                .transpose()?;
            execution.validate(registry.as_deref())?;
        }
        for id in spec
            .dependencies
            .get("source_progress")
            .into_iter()
            .flatten()
        {
            if let Some(handle) = resources.get(id) {
                let progress = handle.get::<QuerySourceProgressResource>()?;
                if progress.0.graph_id() != graph_id
                    || progress.0.query_id() != spec.descriptor.id()
                {
                    anyhow::bail!("source progress belongs to another graph/query");
                }
            }
        }
        Ok(())
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("execution") {
            let execution: QueryExecutionSettings = serde_json::from_value(value.clone())?;
            execution.validate(None)?;
            if !execution.middleware.is_empty()
                && spec
                    .dependencies
                    .get("middleware")
                    .map_or(true, Vec::is_empty)
            {
                anyhow::bail!("query middleware requires a declared middleware resource");
            }
        }
        let literals = spec
            .configuration
            .iter()
            .filter_map(|(key, value)| {
                if let ConfigurationValue::Literal(value) = value {
                    Some((key.clone(), value.clone()))
                } else {
                    None
                }
            })
            .collect();
        if options(&literals)?.recovery == QueryRecoveryPolicy::AutoReset
            && spec
                .dependencies
                .get("bootstrap")
                .map_or(true, Vec::is_empty)
        {
            anyhow::bail!("auto_reset requires a declared bootstrap provider");
        }
        if spec.descriptor != query_descriptor(spec.descriptor.id().clone()) {
            anyhow::bail!("query requires typed graph input and typed query-row output");
        }
        let language_value = spec.configuration.get("language").and_then(|value| {
            if let ConfigurationValue::Literal(value) = value {
                Some(value)
            } else {
                None
            }
        });
        let language = language(language_value)?;
        if !matches!(
            spec.configuration.get("language"),
            Some(ConfigurationValue::Reference { .. })
        ) {
            if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("query") {
                parser(language).0.parse(
                    value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid query text"))?,
                )?;
            }
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid stream"))?,
            )?;
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("outbox_capacity")
        {
            if value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .and_then(NonZeroUsize::new)
                .is_none()
            {
                anyhow::bail!("outbox capacity must be a positive representable integer");
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let provider = context
            .resources::<QueryIndexProviderResource>("indexes")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing index provider"))
            })?;
        let config = context.configuration();
        let text = config
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing query")))?;
        let stream = config
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing stream")))?;
        let capacity = match config.get("outbox_capacity") {
            None => 1000,
            Some(value) => value.as_u64().ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid outbox capacity"))
            })?,
        };
        let capacity = usize::try_from(capacity)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid outbox capacity"))
            })?;
        let definition = ContinuousQueryDefinition {
            graph_id: context.graph_id.to_string(),
            id: context.component_id.clone(),
            query: text.to_owned(),
            language: language(config.get("language")).map_err(ComponentCreationError::terminal)?,
            output_stream: StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?,
            outbox_capacity: capacity,
        };
        let execution: QueryExecutionSettings = config
            .get("execution")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(ComponentCreationError::terminal)?
            .unwrap_or_default();
        let middleware = if context
            .specification
            .dependencies
            .contains_key("middleware")
        {
            context
                .resources::<QueryMiddlewareResource>("middleware")
                .map_err(ComponentCreationError::terminal)?
                .pop()
                .map(|resource| resource.0.clone())
        } else {
            None
        };
        let mut query = ContinuousQueryTransformer::construct(
            definition,
            provider.0.clone(),
            options(config).map_err(ComponentCreationError::terminal)?,
            execution,
            middleware,
            config
                .get("defer_build")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        )
        .await
        .map_err(ComponentCreationError::retryable)?
        .with_recovery_scope(context.instance_id.clone())
        .map_err(ComponentCreationError::terminal)?;
        query.reset_configuration = config
            .get("reset_configuration")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        query.runtime_compatibility = config
            .get("runtime_compatibility")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if context
            .specification
            .dependencies
            .contains_key("scheduling")
        {
            query.scheduling = context
                .resources::<super::QuerySchedulingResource>("scheduling")
                .map_err(ComponentCreationError::terminal)?
                .pop();
        }
        if context.specification.dependencies.contains_key("bootstrap") {
            if let Some(bootstrap) = context
                .resources::<QueryBootstrapResource>("bootstrap")
                .map_err(ComponentCreationError::terminal)?
                .pop()
            {
                query = query.with_bootstrap(bootstrap.0.clone());
            }
        }
        if context
            .specification
            .dependencies
            .contains_key("source_progress")
        {
            if let Some(progress) = context
                .resources::<QuerySourceProgressResource>("source_progress")
                .map_err(ComponentCreationError::terminal)?
                .pop()
            {
                query = query
                    .with_source_progress(progress.0.clone())
                    .map_err(ComponentCreationError::terminal)?;
            }
        }
        if context.specification.dependencies.contains_key("catalog") {
            if let Some(catalog) = context
                .resources::<super::QueryResultsCatalog>("catalog")
                .map_err(ComponentCreationError::terminal)?
                .pop()
            {
                query = query
                    .with_result_catalog(&catalog)
                    .map_err(ComponentCreationError::terminal)?;
            }
        }
        Ok(ConstructedComponent::query(Box::new(
            super::TransactionTransformer::from_query(query),
        )))
    }
}

#[cfg(all(test, feature = "computation-rocksdb-tests"))]
mod profiling_tests {
    use super::*;
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::{IndexSet, SessionControl},
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    };
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::{
        sync::atomic::{AtomicU64, AtomicUsize},
        time::Duration,
    };
    use tokio::sync::Notify;

    #[derive(Default)]
    struct CommitGate {
        remaining: AtomicUsize,
        entered: Notify,
        release: Notify,
        gated_return_ns: AtomicU64,
        last_return_ns: AtomicU64,
    }

    struct GatedSession {
        inner: Arc<dyn SessionControl>,
        gate: Arc<CommitGate>,
    }

    #[async_trait]
    impl SessionControl for GatedSession {
        async fn begin(&self) -> Result<(), IndexError> {
            self.inner.begin().await
        }

        async fn commit(&self) -> Result<(), IndexError> {
            self.inner.commit().await?;
            if self
                .gate
                .remaining
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                == Ok(1)
            {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
                self.gate
                    .gated_return_ns
                    .store(timestamp_ns(), Ordering::Release);
            }
            self.gate
                .last_return_ns
                .store(timestamp_ns(), Ordering::Release);
            Ok(())
        }

        fn rollback(&self) -> Result<(), IndexError> {
            self.inner.rollback()
        }
    }

    struct GatedProvider {
        inner: Arc<dyn ComputationIndexProvider>,
        gate: Arc<CommitGate>,
    }

    #[async_trait]
    impl ComputationIndexProvider for GatedProvider {
        async fn create_indexes(
            &self,
            graph: &str,
            query: &str,
        ) -> Result<ComputationIndexes, IndexError> {
            let original = self.inner.create_indexes(graph, query).await?;
            let indexes = original.indexes();
            let session_control: Arc<dyn SessionControl> = Arc::new(GatedSession {
                inner: indexes.session_control.clone(),
                gate: self.gate.clone(),
            });
            let domain = TransactionDomain::new(session_control.clone());
            let wrapped = ComputationIndexes::try_new(
                IndexSet {
                    element_index: indexes.element_index.clone(),
                    archive_index: indexes.archive_index.clone(),
                    result_index: indexes.result_index.clone(),
                    future_queue: indexes.future_queue.clone(),
                    session_control,
                },
                Some(domain.clone()),
                Some(ComputationResource::participating(
                    original.checkpoint_store().unwrap().clone(),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    original.outbox_writer().unwrap().clone(),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    original.live_results_writer().unwrap().clone(),
                    &domain,
                )),
            )
            .map_err(IndexError::other)?;
            Ok(wrapped.with_cleanup(original.cleanup().unwrap().clone()))
        }

        fn is_volatile(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn live_profiling_waits_for_commit_without_rewriting_persistent_output() {
        for publication in [QueryPublicationMode::Atomic, QueryPublicationMode::NonAtomic] {
            let directory = tempfile::tempdir().unwrap();
            let backend: Arc<dyn ComputationIndexProvider> =
                Arc::new(RocksDbComputationProvider::new(
                    directory.path(),
                    RocksIndexOptions::new(
                        false,
                        false,
                        RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
                    ),
                ));
            let gate = Arc::new(CommitGate::default());
            let definition = ContinuousQueryDefinition {
                graph_id: "post-commit-profiling".into(),
                id: ComponentId::try_new("query").unwrap(),
                query: "MATCH (n:Person) RETURN n.name AS name".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("query/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(8).unwrap(),
            };
            let options = QueryOptions {
                publication,
                recovery: QueryRecoveryPolicy::Strict,
            };
            let mut query = ContinuousQueryTransformer::new_with_options(
                definition.clone(),
                Arc::new(GatedProvider {
                    inner: backend.clone(),
                    gate: gate.clone(),
                }),
                options,
            )
            .await
            .unwrap();
            query.start().await.unwrap();
            let outbox = query
                .query()
                .unwrap()
                .resources()
                .outbox_writer()
                .unwrap()
                .clone();
            let results = query.results();
            let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024).unwrap());
            codec.register_schema(QueryChangeCodec::schema()).unwrap();
            let input = InputEnvelope {
                port: PortId::try_new("in").unwrap(),
                envelope: GraphChangeCodec::encode_source_event(
                    Arc::new(crate::channels::SourceEventWrapper::with_sequence(
                        "people".into(),
                        crate::channels::SourceEvent::Change(SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new("people", "one"),
                                    labels: Arc::from([Arc::from("Person")]),
                                    effective_from: 1_000,
                                },
                                properties: ElementPropertyMap::from(
                                    serde_json::json!({"name":"Alice"}),
                                ),
                            },
                        }),
                        chrono::DateTime::from_timestamp_millis(1_000).unwrap(),
                        1,
                        Some(ProfilingMetadata {
                            source_ns: Some(11),
                            source_receive_ns: Some(12),
                            source_send_ns: Some(13),
                            query_core_return_ns: Some(14),
                            query_send_ns: Some(15),
                            ..Default::default()
                        }),
                    )),
                    &ComponentId::try_new("source-adapter").unwrap(),
                    StreamId::try_new("people/out").unwrap(),
                    100,
                    None,
                )
                .unwrap(),
            };
            // NonAtomic first commits its pending-publication marker, then evaluation.
            gate.remaining.store(
                if publication == QueryPublicationMode::Atomic {
                    1
                } else {
                    2
                },
                Ordering::Release,
            );
            let mut processing = Box::pin(query.transform(input));
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    _ = gate.entered.notified() => {},
                    result = &mut processing => panic!("published before commit returned: {result:?}"),
                }
            })
            .await
            .unwrap();
            assert_eq!(results.snapshot().unwrap().as_of_sequence, 0);
            assert!(results.replay(0).unwrap().is_empty());
            let before_publication = outbox.read_from("query", 0).await.unwrap();
            if publication == QueryPublicationMode::Atomic {
                assert_eq!(before_publication.len(), 1);
                let staged = codec.decode(&before_publication[0].1).unwrap();
                let profiling = QueryChangeCodec::metadata(&staged)
                    .unwrap()
                    .profiling
                    .unwrap();
                assert!(profiling.query_core_return_ns.is_none());
                assert!(profiling.query_send_ns.is_none());
            } else {
                assert!(before_publication.is_empty());
            }

            gate.release.notify_one();
            let emissions = tokio::time::timeout(Duration::from_secs(5), processing)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(emissions.len(), 1);
            let live = QueryChangeCodec::to_legacy_result(&emissions[0].envelope).unwrap();
            let profiling = live.profiling.as_ref().unwrap();
            let core_return = profiling.query_core_return_ns.unwrap();
            let send = profiling.query_send_ns.unwrap();
            assert!(core_return >= gate.gated_return_ns.load(Ordering::Acquire));
            assert!(send >= gate.last_return_ns.load(Ordering::Acquire));
            assert!(send >= core_return);
            assert!(profiling.query_receive_ns.unwrap() <= profiling.query_core_call_ns.unwrap());
            assert!(profiling.query_core_call_ns.unwrap() <= core_return);
            assert_eq!(profiling.source_ns, Some(11));
            assert_eq!(profiling.source_receive_ns, Some(12));
            assert_eq!(profiling.source_send_ns, Some(13));

            let persisted = outbox.read_from("query", 0).await.unwrap();
            assert_eq!(persisted.len(), 1);
            if publication == QueryPublicationMode::Atomic {
                assert_eq!(
                    persisted, before_publication,
                    "publication must not rewrite disk"
                );
            }
            let saved = codec.decode(&persisted[0].1).unwrap();
            assert_eq!(saved.id(), emissions[0].envelope.id());
            assert_eq!(saved.system(), emissions[0].envelope.system());
            let saved = QueryChangeCodec::to_legacy_result(&saved).unwrap();
            let mut expected = saved.profiling.clone().unwrap();
            assert!(expected.query_core_return_ns.is_none());
            assert!(expected.query_send_ns.is_none());
            expected.query_core_return_ns = Some(core_return);
            expected.query_send_ns = Some(send);
            assert_eq!(profiling, &expected);
            assert_eq!(live.results, saved.results);
            assert_eq!(live.metadata, saved.metadata);
            assert_eq!(live.sequence, saved.sequence);
            assert_eq!(
                QueryChangeCodec::metadata(&results.replay(0).unwrap()[0])
                    .unwrap()
                    .profiling,
                live.profiling
            );
            drop(outbox);
            query.stop().await.unwrap();
            drop(query);

            let mut recovered =
                ContinuousQueryTransformer::new_with_options(definition, backend, options)
                    .await
                    .unwrap();
            recovered.start().await.unwrap();
            let replay = recovered.results().replay(0).unwrap();
            assert_eq!(replay.len(), 1);
            assert_eq!(
                QueryChangeCodec::metadata(&replay[0]).unwrap().profiling,
                saved.profiling,
                "recovery preserves unknown completion times rather than fabricating them"
            );
            recovered.stop().await.unwrap();
        }
    }
}
