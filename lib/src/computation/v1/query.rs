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
    ComponentSpecification, ComputationComponent, ConfigurationField, ConfigurationSchema,
    ConfigurationType, ConfigurationValue, ConstructedComponent, ConstructionContext,
    EnvelopeCodec, FactoryDescriptor, GraphChangeCodec, ImplementationIdentity, InputEnvelope,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec,
    QueryOutputMetadata, Record, RecordId, RecordImage, ResourceRequirement, ResourceRole,
    StreamId, SystemMetadata, Transformer, WakeupSource,
};

const CONFIGURATION: &str = "\0computation:query-configuration:v1";
const INPUT_PREFIX: &str = "computation:input:";

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

struct ProcessingGuard<'a> {
    failure: &'a AtomicBool,
    complete: bool,
}
impl Drop for ProcessingGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.failure.store(true, Ordering::Release);
        }
    }
}

struct InputProgress {
    key: String,
    sequence: u64,
    position: Option<Bytes>,
    source_id: String,
    profiling: Option<ProfilingMetadata>,
}

fn input_progress(input: &super::ChangeEnvelope) -> anyhow::Result<InputProgress> {
    let raw = GraphChangeCodec::source_metadata(input)?;
    let source_id = raw
        .as_ref()
        .map(|metadata| metadata.source_id.clone())
        .unwrap_or_else(|| input.system().stream().as_str().to_owned());
    let stable_source = raw.as_ref().and_then(|metadata| metadata.sequence);
    let key = if stable_source.is_some() {
        format!("source:{source_id}")
    } else {
        format!("stream:{}", input.system().stream())
    };
    Ok(InputProgress {
        key: format!(
            "{INPUT_PREFIX}{}",
            key.bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
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

pub struct ContinuousQueryTransformer {
    definition: ContinuousQueryDefinition,
    descriptor: ComponentDescriptor,
    provider: Arc<dyn ComputationIndexProvider>,
    query: Option<ComputationQuery>,
    results: QueryResults,
    codec: EnvelopeCodec,
    failure: AtomicBool,
}

impl ContinuousQueryTransformer {
    pub async fn new(
        definition: ContinuousQueryDefinition,
        provider: Arc<dyn ComputationIndexProvider>,
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
        };
        let mut instance = Self {
            descriptor: definition.descriptor(),
            definition,
            provider,
            query: None,
            results,
            codec,
            failure: AtomicBool::new(false),
        };
        instance.build().await?;
        Ok(instance)
    }

    pub fn results(&self) -> QueryResults {
        self.results.clone()
    }

    async fn build(&mut self) -> anyhow::Result<()> {
        let resources = self
            .provider
            .create_indexes(&self.definition.graph_id, self.definition.id.as_str())
            .await?;
        if !self.provider.is_volatile() {
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
        let configuration = self.definition.configuration_bytes()?;
        let stored = checkpoint.read_checkpoint(CONFIGURATION).await?;
        if let Some(stored) = stored {
            if stored.source_position.as_ref() != Some(&configuration) {
                anyhow::bail!(
                    "query configuration changed; explicit reset/rebootstrap is required"
                );
            }
        } else {
            let session = query.resources().indexes().session_control.clone();
            session.begin().await?;
            let staged = checkpoint
                .stage_checkpoint(CONFIGURATION, 1, Some(&configuration))
                .await;
            if let Err(error) = staged {
                session.rollback()?;
                return Err(error.into());
            }
            session.commit().await?;
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
            anyhow::bail!(
                "inconsistent durable query output: state exists without a committed sequence"
            );
        }
        let sequence = sequence.unwrap_or(0);
        if sequence > 0 && outbox.last().map(|(sequence, _)| *sequence) != Some(sequence) {
            anyhow::bail!(
                "inconsistent durable query output: retained tail differs from committed sequence"
            );
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
                anyhow::bail!("inconsistent retained query history has an interior gap");
            }
            let envelope = self.codec.decode(&bytes)?;
            if envelope.system().sequence() != position
                || envelope.system().stream() != &self.definition.output_stream
                || QueryChangeCodec::metadata(&envelope)?.query_id != self.definition.id.as_str()
            {
                anyhow::bail!("retained output identity differs from the query");
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
                anyhow::bail!("durable snapshot contradicts the retained output history");
            }
        }
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .hydrate(rows, sequence, recovered);
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
        resources
            .live_results_writer()
            .ok_or(IndexError::NotSupported)?
            .apply_mutations(id, &mutations)
            .await?;
        resources
            .checkpoint_store()
            .ok_or(IndexError::NotSupported)?
            .write_result_sequence(id, output.system().sequence())
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
        let progress = input_progress(&input.envelope)?;
        if let Some(checkpoint) = query.resources().checkpoint_store() {
            if checkpoint
                .read_checkpoint(&progress.key)
                .await?
                .is_some_and(|saved| saved.sequence >= progress.sequence)
            {
                return Ok(Vec::new());
            }
        }
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let sequence = self.next_sequence()?;
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
                profiling.query_core_return_ns = Some(timestamp_ns());
                profiling.query_send_ns = Some(timestamp_ns());
                let output = QueryChangeCodec::encode_evaluation(
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
                if let Some(checkpoint) = query.resources().checkpoint_store() {
                    checkpoint
                        .stage_checkpoint(
                            &progress.key,
                            progress.sequence,
                            progress.position.as_ref(),
                        )
                        .await?;
                    if let Some(output) = &output {
                        self.stage_output(output).await?;
                    }
                }
                *prepared.lock().map_err(|_| IndexError::CorruptedData)? = output;
                Ok(())
            }
        };
        match query.resources().atomic_result_transaction() {
            Ok(transaction) => {
                query
                    .process_source_changes_with_result_hook(changes, &transaction, hook)
                    .await?;
            }
            Err(_) if self.provider.is_volatile() => {
                query
                    .process_source_changes_with_non_atomic_result_hook(changes, hook)
                    .await?;
            }
            Err(error) => return Err(error.into()),
        }
        self.publish(
            prepared
                .into_inner()
                .map_err(|_| anyhow::anyhow!("prepared output poisoned"))?,
        )
    }
}

#[async_trait]
impl ComputationComponent for ContinuousQueryTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        if self.failure.load(Ordering::Acquire) && self.provider.is_volatile() {
            anyhow::bail!("failed volatile query requires explicit reconstruction and bootstrap");
        }
        if self.query.is_none() {
            self.build().await?;
        }
        let mut guard = ProcessingGuard {
            failure: &self.failure,
            complete: false,
        };
        self.recover().await?;
        guard.complete = true;
        self.failure.store(false, Ordering::Release);
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
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
            failure: &self.failure,
            complete: false,
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
            failure: &self.failure,
            complete: false,
        };
        let query = self.query()?;
        let sequence = self.next_sequence()?;
        let prepared = Mutex::new(None);
        let owner = &*self;
        let hook = |due: Arc<drasi_core::computation::ComputationFutureResult>| {
            let prepared = &prepared;
            async move {
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
                let output = QueryChangeCodec::encode_evaluation(
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
                if !owner.provider.is_volatile() {
                    if let Some(output) = &output {
                        owner.stage_output(output).await?;
                    }
                }
                *prepared.lock().map_err(|_| IndexError::CorruptedData)? = output;
                Ok(())
            }
        };
        match query.resources().atomic_result_transaction() {
            Ok(transaction) => {
                query
                    .process_due_futures_with_result_hook(&transaction, hook)
                    .await?;
            }
            Err(_) if self.provider.is_volatile() => {
                query
                    .process_due_futures_with_non_atomic_result_hook(hook)
                    .await?;
            }
            Err(error) => return Err(error.into()),
        }
        let result = self.publish(
            prepared
                .into_inner()
                .map_err(|_| anyhow::anyhow!("prepared future output poisoned"))?,
        );
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
                dependencies: BTreeMap::from([(
                    Arc::from("indexes"),
                    ResourceRequirement::exactly_one::<QueryIndexProviderResource>(
                        ResourceRole::IndexBackend,
                    ),
                )]),
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

#[async_trait]
impl ComponentFactory for ContinuousQueryFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
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
        let query = ContinuousQueryTransformer::new(definition, provider.0.clone())
            .await
            .map_err(ComponentCreationError::retryable)?;
        Ok(ConstructedComponent::query(Box::new(query)))
    }
}
