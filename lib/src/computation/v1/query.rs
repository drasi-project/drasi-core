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
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
};

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
    QueryHistoryError, QueryResults, QuerySnapshot,
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
use super::{
    QuerySourceProgress, QuerySourceProgressResource, SourceProgressKey, SourceProgressSnapshot,
};

const CONFIGURATION: &str = "\0computation:query-configuration:v1";
const INPUT_PREFIX: &str = "computation:input:";
const BOOTSTRAP: &str = "\0computation:query-bootstrap:v1";
const PENDING_OUTPUT: &str = "\0computation:pending-output:v1";

#[path = "query_recovery.rs"]
mod recovery;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ComputationQueryLanguage {
    #[default]
    Cypher,
    Gql,
}

#[derive(Debug, Clone)]
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

    fn configuration_bytes(&self) -> anyhow::Result<Bytes> {
        Ok(Bytes::from(serde_json::to_vec(&(
            1u32,
            &self.query,
            self.language,
            self.output_stream.as_str(),
        ))?))
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

struct FutureWakeup(Arc<dyn FutureQueue>);

#[async_trait]
impl WakeupSource for FutureWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            if let Some(due) = self.0.peek_due_time().await? {
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
        Ok(self.0.peek_due_time().await?.is_some())
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

struct InputProgress {
    key: String,
    sequence: u64,
    position: Option<Bytes>,
    source_id: String,
    profiling: Option<ProfilingMetadata>,
    identity: SourceProgressKey,
}

fn input_progress(input: &super::ChangeEnvelope) -> anyhow::Result<InputProgress> {
    let raw = GraphChangeCodec::source_metadata(input)?;
    let source_id = raw
        .as_ref()
        .map(|metadata| metadata.source_id.clone())
        .unwrap_or_else(|| input.system().stream().as_str().to_owned());
    let stable_source = raw.as_ref().and_then(|metadata| metadata.sequence);
    Ok(InputProgress {
        identity: if stable_source.is_some() {
            SourceProgressKey::Source(source_id.clone())
        } else {
            SourceProgressKey::Stream(input.system().stream().clone())
        },
        key: progress_key(
            input.system().stream().as_str(),
            stable_source.map(|_| source_id.as_str()),
        ),
        sequence: stable_source.unwrap_or_else(|| input.system().sequence()),
        position: raw
            .as_ref()
            .and_then(|metadata| {
                metadata
                    .source_position
                    .as_ref()
                    .map(|bytes| Bytes::copy_from_slice(bytes))
            })
            .or_else(|| input.system().source_position().cloned()),
        source_id,
        profiling: raw.and_then(|metadata| metadata.profiling),
    })
}

fn progress_key(stream: &str, source: Option<&str>) -> String {
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
        };
        instance.build().await?;
        Ok(instance)
    }

    pub fn results(&self) -> QueryResults {
        self.results.clone()
    }

    pub fn with_bootstrap(mut self, provider: Arc<dyn ComputationBootstrapProvider>) -> Self {
        self.bootstrap = Some(provider);
        self
    }

    pub fn with_source_progress(
        mut self,
        progress: Arc<QuerySourceProgress>,
    ) -> anyhow::Result<Self> {
        if progress.graph_id() != self.definition.graph_id
            || progress.query_id() != &self.definition.id
        {
            anyhow::bail!("source progress belongs to another graph/query");
        }
        self.source_progress = Some(progress);
        Ok(self)
    }

    async fn publish_source_progress(&self) -> anyhow::Result<()> {
        let Some(progress) = &self.source_progress else {
            return Ok(());
        };
        let checkpoint = self
            .query()?
            .resources()
            .checkpoint_store()
            .ok_or_else(|| {
                anyhow::anyhow!("source resume requires an actual query checkpoint store")
            })?;
        if !checkpoint.is_persistent() || self.provider.is_volatile() {
            anyhow::bail!("source resume cannot use volatile query progress");
        }
        let mut checkpoints = BTreeMap::new();
        for (key, checkpoint) in checkpoint.read_all_checkpoints().await? {
            let Some(key) = key.strip_prefix(INPUT_PREFIX) else {
                continue;
            };
            if key.len() % 2 != 0 || !key.is_ascii() {
                anyhow::bail!("invalid source checkpoint identity");
            }
            let decoded = (0..key.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(&key[index..index + 2], 16))
                .collect::<Result<Vec<_>, _>>()?;
            let decoded = String::from_utf8(decoded)?;
            let key = if let Some(source) = decoded.strip_prefix("source:") {
                SourceProgressKey::Source(source.to_owned())
            } else if let Some(stream) = decoded.strip_prefix("stream:") {
                SourceProgressKey::Stream(StreamId::try_new(stream)?)
            } else {
                anyhow::bail!("invalid source checkpoint identity");
            };
            checkpoints.insert(key, checkpoint);
        }
        progress.publish(SourceProgressSnapshot {
            ready: true,
            persistent: true,
            reset_generation: self.results.snapshot()?.generation,
            checkpoints,
            failure: None,
        });
        Ok(())
    }

    async fn build(&mut self) -> anyhow::Result<()> {
        let resources = self
            .provider
            .create_indexes(&self.definition.graph_id, self.definition.id.as_str())
            .await?;
        if !self.provider.is_volatile() && self.options.publication == QueryPublicationMode::Atomic
        {
            resources.atomic_result_transaction()?;
        }
        let (parser, functions) = parser(self.definition.language);
        self.query = Some(
            ComputationQuery::try_build(
                QueryBuilder::new(&self.definition.query, parser).with_function_registry(functions),
                resources,
            )
            .await?,
        );
        Ok(())
    }

    fn query(&self) -> anyhow::Result<&ComputationQuery> {
        self.query
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query requires reconstruction"))
    }

    async fn recover(&self) -> anyhow::Result<()> {
        let query = self.query()?;
        let Some(checkpoint) = query.resources().checkpoint_store() else {
            return Ok(());
        };
        if let Some(marker) = checkpoint.read_checkpoint(BOOTSTRAP).await? {
            if marker.sequence > 1 || marker.source_position.is_some() {
                return Err(
                    QueryRecoveryError::Inconsistent("invalid bootstrap marker".into()).into(),
                );
            }
            if marker.sequence == 0 {
                return Err(QueryRecoveryError::IncompleteBootstrap.into());
            }
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
        let configuration = self.definition.configuration_bytes()?;
        let stored = checkpoint.read_checkpoint(CONFIGURATION).await?;
        if let Some(stored) = stored {
            if stored.source_position.as_ref() != Some(&configuration) {
                return Err(QueryRecoveryError::ConfigurationChanged.into());
            }
        } else {
            query
                .resource_transaction(|| async {
                    checkpoint
                        .stage_checkpoint(CONFIGURATION, 1, Some(&configuration))
                        .await
                })
                .await?;
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
            )?;
            rows.insert(row.identity().clone(), row);
        }
        let mut recovered = Vec::new();
        let mut previous = None;
        let mut expected = HashMap::new();
        for (position, bytes) in outbox {
            if previous.is_some_and(|previous: u64| previous.checked_add(1) != Some(position)) {
                return Err(QueryRecoveryError::Inconsistent(
                    "retained history has an interior gap".into(),
                )
                .into());
            }
            let envelope = self.codec.decode(&bytes)?;
            if envelope.system().sequence() != position
                || envelope.system().stream() != &self.definition.output_stream
                || QueryChangeCodec::metadata(&envelope)?.query_id != self.definition.id.as_str()
            {
                return Err(QueryRecoveryError::Inconsistent(
                    "retained output identity differs from the query".into(),
                )
                .into());
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
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .hydrate(
                rows,
                sequence,
                recovered,
                reset.map(|marker| marker.generation).unwrap_or(0),
            );
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
        resources
            .outbox_writer()
            .ok_or(IndexError::NotSupported)?
            .append(id, output.system().sequence(), &bytes)
            .await?;
        resources
            .outbox_writer()
            .ok_or(IndexError::NotSupported)?
            .trim_to_capacity(id, self.definition.outbox_capacity.get())
            .await?;
        self.stage_projection(output).await?;
        resources
            .checkpoint_store()
            .ok_or(IndexError::NotSupported)?
            .write_result_sequence(id, output.system().sequence())
            .await
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
                Ok(vec![OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: output,
                }])
            }
            None => Ok(Vec::new()),
        }
    }

    async fn process(&self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        if self.failure.load(Ordering::Acquire) {
            anyhow::bail!("query processing is fenced pending recovery");
        }
        let query = self.query()?;
        let mut progress = input_progress(&input.envelope)?;
        if self
            .watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
            .get(&progress.key)
            .is_some_and(|saved| *saved >= progress.sequence)
        {
            return Ok(Vec::new());
        }
        if let Some(checkpoint) = query.resources().checkpoint_store() {
            let saved = checkpoint.read_checkpoint(&progress.key).await?;
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
        let prepared = Mutex::new(None);
        let mut profiling = progress.profiling.clone().unwrap_or_default();
        profiling.query_receive_ns = Some(timestamp_ns());
        profiling.query_core_call_ns = Some(timestamp_ns());
        let hook = |results: Arc<[drasi_core::evaluation::context::QueryPartEvaluationContext]>| {
            let prepared = &prepared;
            let input = &input;
            let progress = &progress;
            let mut profiling = profiling.clone();
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
                profiling.query_core_return_ns = Some(timestamp_ns());
                profiling.query_send_ns = Some(timestamp_ns());
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
                        metadata: HashMap::new(),
                        profiling: Some(profiling),
                    },
                )
                .map_err(IndexError::other)?;
                if let Some(output) = &mut output {
                    QueryChangeCodec::set_generation(
                        output,
                        &self.definition.id,
                        self.results
                            .snapshot()
                            .map_err(IndexError::other)?
                            .generation,
                    )
                    .map_err(IndexError::other)?;
                }
                if let Some(checkpoint) = query.resources().checkpoint_store() {
                    checkpoint
                        .stage_checkpoint(
                            &progress.key,
                            progress.sequence,
                            progress.position.as_ref(),
                        )
                        .await?;
                    if let Some(output) = &output {
                        if self.options.publication == QueryPublicationMode::Atomic {
                            self.stage_output(output).await?;
                        } else {
                            checkpoint
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
        let output = prepared
            .into_inner()
            .map_err(|_| anyhow::anyhow!("prepared output poisoned"))?;
        if self.options.publication == QueryPublicationMode::NonAtomic
            && !self.provider.is_volatile()
        {
            if let Some(output) = &output {
                self.finish_non_atomic(output).await?;
            }
        }
        self.watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
            .insert(progress.key, progress.sequence);
        if let Some(confirmation) = &self.source_progress {
            confirmation.confirm(
                progress.identity,
                SourceCheckpoint::new(progress.sequence, progress.position),
            );
        }
        self.publish(output)
    }
}

#[async_trait]
impl ComputationComponent for ContinuousQueryTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
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
        self.publish_source_progress().await?;
        guard.complete = true;
        self.failure.store(false, Ordering::Release);
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .ready = true;
        self.results.notify.notify_waiters();
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
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
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let mut guard = ProcessingGuard {
            failure: self.failure.clone(),
            complete: false,
            progress: self.source_progress.clone(),
        };
        let result = self.process(input).await;
        guard.complete = result.is_ok();
        result
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        self.query
            .as_ref()
            .map(|query| Arc::new(FutureWakeup(query.future_queue())) as Arc<dyn WakeupSource>)
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if self.failure.load(Ordering::Acquire) {
            anyhow::bail!("query processing is fenced pending recovery");
        }
        let mut guard = ProcessingGuard {
            failure: self.failure.clone(),
            complete: false,
            progress: self.source_progress.clone(),
        };
        let query = self.query()?;
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
                        timestamp,
                        metadata: HashMap::new(),
                        profiling: None,
                    },
                )
                .map_err(IndexError::other)?;
                if let Some(output) = &mut output {
                    QueryChangeCodec::set_generation(
                        output,
                        &owner.definition.id,
                        owner
                            .results
                            .snapshot()
                            .map_err(IndexError::other)?
                            .generation,
                    )
                    .map_err(IndexError::other)?;
                }
                if !owner.provider.is_volatile() {
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
        if self.options.publication == QueryPublicationMode::NonAtomic
            && !self.provider.is_volatile()
        {
            if let Some(output) = &output {
                self.finish_non_atomic(output).await?;
            }
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
        let mut query = ContinuousQueryTransformer::new_with_options(
            definition,
            provider.0.clone(),
            options(config).map_err(ComponentCreationError::terminal)?,
        )
        .await
        .map_err(ComponentCreationError::retryable)?;
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
        Ok(ConstructedComponent::query(Box::new(query)))
    }
}
