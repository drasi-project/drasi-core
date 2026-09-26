// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Context;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{computation::ComputationIndexProvider, middleware::MiddlewareTypeRegistry};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::{middleware_recovery::TransformStore, *};

const IMPLEMENTATION: &str = "drasi/transaction-transformer";
const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

/// Explicit opt-in to execution inside another component's transaction.
///
/// Ordinary `Transformer` execution is unchanged. The container calls only this
/// method, not the step's standalone lifecycle, transform, wakeup or delivery
/// hooks. Implementations must use the borrowed context for all mutable business
/// state, perform no external side effects, start no workers, and never commit.
/// Immutable configuration and computation-only caches may remain on `self`.
///
/// One input batch produces one output batch. Filtering uses an empty change set;
/// expansion uses multiple ordered operations in that batch. Nothing is published
/// outside the container until all steps and the shared transaction succeed.
#[async_trait]
pub trait TransactionalTransformer: Transformer {
    fn transaction_input_schema(&self) -> Arc<Schema>;
    fn transaction_output_schema(&self) -> Arc<Schema>;

    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &TransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionStepDefinition {
    pub id: ComponentId,
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub configuration: serde_json::Value,
}

/// Configuration-only construction, with no I/O, workers, storage or activation.
/// The return type makes participation an implementation contract, not a flag
/// that could incorrectly upgrade an ordinary transformer to transactional use.
pub trait TransactionalTransformerFactory: Send + Sync {
    fn implementation(&self) -> ImplementationIdentity;
    fn configuration_version(&self) -> u32;
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> anyhow::Result<Box<dyn TransactionalTransformer>>;
}

#[derive(Default)]
pub struct TransactionalTransformerRegistry {
    factories: BTreeMap<ImplementationIdentity, Arc<dyn TransactionalTransformerFactory>>,
}

impl TransactionalTransformerRegistry {
    pub fn standard(middleware: Arc<MiddlewareTypeRegistry>) -> Self {
        let mut registry = Self::default();
        registry
            .register(Arc::new(TransactionalMiddlewareFactory(middleware)))
            .expect("unique built-in transactional transformer");
        registry
    }

    pub fn register(
        &mut self,
        factory: Arc<dyn TransactionalTransformerFactory>,
    ) -> anyhow::Result<()> {
        let implementation = factory.implementation();
        data::validate_identifier("transaction implementation", &implementation.name)?;
        data::validate_identifier(
            "transaction implementation version",
            &implementation.version,
        )?;
        if let Some(plugin) = &implementation.plugin {
            data::validate_identifier("transaction plugin", &plugin.id)?;
            data::validate_identifier("transaction plugin version", &plugin.version)?;
        }
        anyhow::ensure!(
            factory.configuration_version() > 0,
            "invalid step configuration version"
        );
        anyhow::ensure!(
            !self.factories.contains_key(&implementation),
            "duplicate transactional transformer implementation"
        );
        self.factories.insert(implementation, factory);
        Ok(())
    }

    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> anyhow::Result<Box<dyn TransactionalTransformer>> {
        let factory = self
            .factories
            .get(&definition.implementation)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "implementation {} does not have a registered transactional factory",
                    definition.implementation.name
                )
            })?;
        anyhow::ensure!(
            factory.configuration_version() == definition.configuration_version,
            "unsupported transactional step configuration version"
        );
        factory
            .create(definition)
            .with_context(|| format!("construct transaction step {}", definition.id))
    }
}

pub struct TransactionalTransformerRegistryResource(pub Arc<TransactionalTransformerRegistry>);

/// A linear sequence and its shared persistent transaction/storage boundary.
/// Step IDs select isolated storage areas and must remain stable on reopen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionTransformerDefinition {
    pub graph_id: String,
    pub id: ComponentId,
    pub output_stream: StreamId,
    pub steps: Vec<TransactionStepDefinition>,
    pub outbox_capacity: NonZeroUsize,
}

impl TransactionTransformerDefinition {
    pub fn descriptor(
        &self,
        registry: &TransactionalTransformerRegistry,
    ) -> anyhow::Result<ComponentDescriptor> {
        Ok(Prepared::new(self, registry)?.descriptor)
    }

    pub fn specification(
        &self,
        registry: &TransactionalTransformerRegistry,
        registry_id: ResourceId,
        indexes_id: ResourceId,
    ) -> anyhow::Result<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: self.descriptor(registry)?,
            role: ComponentRole::Transformer,
            completion: None,
            implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")?,
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("transaction"),
                ConfigurationValue::Literal(serde_json::to_value(self)?),
            )]),
            dependencies: BTreeMap::from([
                (Arc::from("transformers"), vec![registry_id]),
                (Arc::from("indexes"), vec![indexes_id]),
            ]),
        })
    }
}

struct Step {
    definition: TransactionStepDefinition,
    descriptor: ComponentDescriptor,
    input: Arc<Schema>,
    output: Arc<Schema>,
    transformer: Box<dyn TransactionalTransformer>,
}

struct Prepared {
    steps: Vec<Step>,
    descriptor: ComponentDescriptor,
    codec: EnvelopeCodec,
    schemas: Vec<Arc<Schema>>,
}

impl Prepared {
    fn new(
        definition: &TransactionTransformerDefinition,
        registry: &TransactionalTransformerRegistry,
    ) -> anyhow::Result<Self> {
        data::validate_identifier("transaction graph", &definition.graph_id)?;
        anyhow::ensure!(
            !definition.steps.is_empty(),
            "transaction sequence must not be empty"
        );
        let mut ids = BTreeSet::new();
        let mut schemas: BTreeMap<(SchemaId, u32), Arc<Schema>> = BTreeMap::new();
        let mut steps: Vec<Step> = Vec::new();
        for config in &definition.steps {
            anyhow::ensure!(
                ids.insert(config.id.clone()),
                "duplicate transaction step ID"
            );
            let transformer = registry.create(config)?;
            let descriptor = transformer.descriptor().clone();
            anyhow::ensure!(
                descriptor.id() == &config.id,
                "transaction step descriptor ID mismatch"
            );
            anyhow::ensure!(
                !transformer.requires_readiness_confirmation()
                    && transformer.wakeup_source().is_none()
                    && !transformer.has_pending_emissions(),
                "transaction steps cannot run independent wakeups or pending work"
            );
            let inputs: Vec<_> = descriptor
                .ports()
                .iter()
                .filter(|port| port.direction() == PortDirection::Input)
                .collect();
            let outputs: Vec<_> = descriptor
                .ports()
                .iter()
                .filter(|port| port.direction() == PortDirection::Output)
                .collect();
            anyhow::ensure!(
                inputs.len() == 1 && outputs.len() == 1,
                "transaction steps require exactly one input and one output"
            );
            let input = transformer.transaction_input_schema();
            let output = transformer.transaction_output_schema();
            data::validate_schema(inputs[0].schema(), input.descriptor())?;
            data::validate_schema(outputs[0].schema(), output.descriptor())?;
            if let Some(previous) = steps.last() {
                data::validate_schema(previous.output.descriptor(), input.descriptor())?;
            }
            for schema in [&input, &output] {
                let key = (
                    schema.descriptor().id().clone(),
                    schema.descriptor().version().value(),
                );
                if let Some(previous) = schemas.get(&key) {
                    data::validate_schema(previous.descriptor(), schema.descriptor())?;
                } else {
                    schemas.insert(key, schema.clone());
                }
            }
            steps.push(Step {
                definition: config.clone(),
                descriptor,
                input,
                output,
                transformer,
            });
        }
        let descriptor = ComponentDescriptor::try_new(
            definition.id.clone(),
            vec![
                PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    steps[0].input.descriptor().clone(),
                    PipeRequirements::new([
                        PipeCapability::FifoPerStream,
                        PipeCapability::Backpressure,
                    ]),
                ),
                PortDescriptor::new(
                    PortId::try_new("out")?,
                    PortDirection::Output,
                    steps
                        .last()
                        .expect("nonempty sequence")
                        .output
                        .descriptor()
                        .clone(),
                    PipeRequirements::new([
                        PipeCapability::FifoPerStream,
                        PipeCapability::Backpressure,
                        PipeCapability::DurableAcceptance,
                        PipeCapability::Replay,
                        PipeCapability::ExplicitAcknowledgement,
                    ]),
                ),
            ],
        )?;
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(MAX_BATCH_BYTES).expect("limit"));
        let schemas: Vec<_> = schemas.into_values().collect();
        for schema in &schemas {
            codec.register_schema(schema.clone())?;
        }
        Ok(Self {
            steps,
            descriptor,
            codec,
            schemas,
        })
    }
}

#[derive(Default)]
struct TransactionWakeup {
    pending: AtomicBool,
    notify: Notify,
}

impl TransactionWakeup {
    fn set(&self, pending: bool) {
        self.pending.store(pending, Ordering::Release);
        self.notify.notify_waiters();
    }
}

#[async_trait]
impl WakeupSource for TransactionWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending.load(Ordering::Acquire) {
                return Ok(());
            }
            notified.await;
        }
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.pending.load(Ordering::Acquire))
    }
}

struct RunGuard {
    failed: Arc<AtomicBool>,
    progress: Option<Arc<QuerySourceProgress>>,
    complete: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.failed.store(true, Ordering::Release);
            if let Some(progress) = &self.progress {
                progress.fail();
            }
        }
    }
}

/// One graph component, one shared commit, no pipes between its ordered steps.
/// State, input progress and final output commit before the output is returned.
/// Failed/cancelled processing requires reconstruction; retained committed output
/// is replayed without invoking any of the steps again.
struct LinearTransaction {
    definition: TransactionTransformerDefinition,
    prepared: Prepared,
    store: TransformStore,
    step_stream_prefix: String,
    pending: VecDeque<u64>,
    deferred: Option<InputEnvelope>,
    wakeup: Arc<TransactionWakeup>,
    progress: Option<Arc<QuerySourceProgress>>,
    failed: Arc<AtomicBool>,
    running: bool,
}

impl LinearTransaction {
    pub async fn new(
        definition: TransactionTransformerDefinition,
        registry: Arc<TransactionalTransformerRegistry>,
        provider: Arc<dyn ComputationIndexProvider>,
    ) -> anyhow::Result<Self> {
        let scope = definition.graph_id.clone();
        Self::new_scoped(definition, registry, provider, scope).await
    }

    async fn new_scoped(
        definition: TransactionTransformerDefinition,
        registry: Arc<TransactionalTransformerRegistry>,
        provider: Arc<dyn ComputationIndexProvider>,
        scope: String,
    ) -> anyhow::Result<Self> {
        let prepared = Prepared::new(&definition, &registry)?;
        anyhow::ensure!(
            !provider.is_volatile(),
            "transaction transformer requires persistent atomic storage"
        );
        let indexes = provider
            .create_indexes(&definition.graph_id, definition.id.as_str())
            .await?;
        let store = TransformStore::configured(
            indexes,
            definition.id.clone(),
            definition.output_stream.clone(),
            DurableMiddlewareOptions {
                graph_id: definition.graph_id.clone(),
                outbox_capacity: definition.outbox_capacity,
            },
            scope.clone(),
            serde_json::to_value((
                IMPLEMENTATION,
                1u32,
                &scope,
                &definition,
                &prepared.descriptor,
                prepared
                    .steps
                    .iter()
                    .map(|step| &step.descriptor)
                    .collect::<Vec<_>>(),
            ))?,
            prepared
                .steps
                .last()
                .expect("nonempty sequence")
                .output
                .clone(),
            prepared.schemas.clone(),
        )?;
        let step_stream_prefix = format!(
            "transaction-step/{}/{}/{}",
            entities::encode_entity_key(&scope),
            entities::encode_entity_key(&definition.graph_id),
            entities::encode_entity_key(definition.id.as_str()),
        );
        Ok(Self {
            definition,
            prepared,
            store,
            step_stream_prefix,
            pending: VecDeque::new(),
            deferred: None,
            wakeup: Arc::new(TransactionWakeup::default()),
            progress: None,
            failed: Arc::new(AtomicBool::new(false)),
            running: false,
        })
    }

    pub fn step_descriptors(&self) -> impl Iterator<Item = &ComponentDescriptor> {
        self.prepared.steps.iter().map(|step| &step.descriptor)
    }

    pub fn with_source_progress(
        mut self,
        progress: Arc<QuerySourceProgress>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !self.running,
            "bind transaction source progress before start"
        );
        anyhow::ensure!(
            progress.graph_id() == self.definition.graph_id
                && progress.component_id() == &self.definition.id,
            "source progress belongs to another transaction transformer"
        );
        progress.require_stream_replay();
        progress.pending();
        self.progress = Some(progress);
        Ok(self)
    }

    fn guard(&self) -> RunGuard {
        RunGuard {
            failed: self.failed.clone(),
            progress: self.progress.clone(),
            complete: false,
        }
    }

    fn check_running(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.running, "transaction transformer is not running");
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "transaction transformer requires reconstruction after failed or cancelled processing"
        );
        Ok(())
    }

    fn outputs(envelope: ChangeEnvelope) -> Vec<OutputEnvelope> {
        vec![OutputEnvelope {
            port: PortId::try_new("out").expect("port"),
            envelope,
        }]
    }

    async fn process(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            input.port.as_str() == "in",
            "unknown transaction input port"
        );
        let prepared = &self.prepared;
        let stream = self.definition.output_stream.clone();
        let step_stream_prefix = &self.step_stream_prefix;
        let envelope = &input.envelope;
        data::validate_schema(
            prepared.steps[0].input.descriptor(),
            envelope.changes().schema(),
        )?;
        prepared.codec.decode(&prepared.codec.encode(envelope)?)?;
        let output = self
            .store
            .process_envelope(envelope, |elements, emission| async move {
                let mut current = envelope.clone();
                for step in &prepared.steps {
                    anyhow::ensure!(
                        step.transformer.descriptor() == &step.descriptor,
                        "transaction step changed its descriptor"
                    );
                    data::validate_schema(step.input.descriptor(), current.changes().schema())?;
                    let context = TransactionContext::new(
                        &step.definition.id,
                        elements.as_ref(),
                        StreamId::try_new(format!(
                            "{step_stream_prefix}/{}",
                            entities::encode_entity_key(step.definition.id.as_str()),
                        ))?,
                        emission,
                    );
                    current = step
                        .transformer
                        .transform_in_transaction(current, &context)
                        .await
                        .with_context(|| {
                            format!("transaction step {} failed", step.definition.id)
                        })?;
                    context.validate()?;
                    data::validate_schema(step.output.descriptor(), current.changes().schema())?;
                    current = context.finish(current)?;
                    // Decode through the registered validator before a later step can
                    // consume records produced with a permissive or foreign validator.
                    prepared.codec.decode(&prepared.codec.encode(&current)?)?;
                }
                let changes = ChangeSet::try_new(
                    ChangeSetId::try_new(
                        stream.as_str(),
                        Bytes::copy_from_slice(&emission.to_be_bytes()),
                    )?,
                    current.changes().schema().clone(),
                    current.changes().operations().to_vec(),
                )?;
                let mut system = SystemMetadata::new(stream.clone(), emission);
                if let Some(timestamp) = envelope.system().timestamp() {
                    system = system.with_timestamp(timestamp);
                }
                if let Some(position) = envelope.system().source_position() {
                    system = system.with_source_position(position.clone());
                }
                Ok(current.derive(emission_id(&stream, emission)?, changes, system))
            })
            .await?;
        if let Some(progress) = &self.progress {
            progress.publish(self.store.progress()?);
        }
        Ok(output.map_or_else(Vec::new, Self::outputs))
    }

    async fn continue_pending(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if let Some(sequence) = self.pending.front().copied() {
            let envelope = self.store.replay(sequence).await?;
            self.pending.pop_front();
            self.wakeup
                .set(!self.pending.is_empty() || self.deferred.is_some());
            return Ok(Self::outputs(envelope));
        }
        self.wakeup.set(false);
        match self.deferred.take() {
            Some(input) => self.process(input).await,
            None => Ok(Vec::new()),
        }
    }

    fn completed(result: &anyhow::Result<Vec<OutputEnvelope>>) -> bool {
        result.as_ref().map_or_else(
            |error| {
                matches!(
                    error.downcast_ref::<MiddlewareRecoveryError>(),
                    Some(MiddlewareRecoveryError::RetentionExhausted)
                )
            },
            |_| true,
        )
    }
}

#[async_trait]
impl ComputationComponent for LinearTransaction {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.prepared.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"transaction": self.definition}))
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "failed transaction transformer requires reconstruction"
        );
        if self.running {
            return Ok(());
        }
        let mut guard = self.guard();
        if let Some(progress) = &self.progress {
            progress.pending();
        }
        self.store.recover().await?;
        self.pending = self.store.pending()?;
        self.deferred = None;
        self.wakeup.set(!self.pending.is_empty());
        if let Some(progress) = &self.progress {
            progress.publish(self.store.progress()?);
        }
        self.running = true;
        guard.complete = true;
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        self.wakeup.set(false);
        if self.failed.load(Ordering::Acquire) || self.store.transaction.recovery_required() {
            if let Some(progress) = &self.progress {
                progress.fail();
            }
            self.store.transaction.shutdown().await?;
        } else {
            let mut guard = self.guard();
            self.store.transaction.quiesce().await?;
            if let Some(progress) = &self.progress {
                progress.pending();
            }
            guard.complete = true;
        }
        Ok(())
    }
}

#[async_trait]
impl Transformer for LinearTransaction {
    fn has_pending_emissions(&self) -> bool {
        !self.pending.is_empty() || self.deferred.is_some()
    }
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.wakeup.clone())
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.continue_transform().await
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        let mut guard = self.guard();
        let result = self.continue_pending().await;
        guard.complete = Self::completed(&result);
        result
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        anyhow::ensure!(
            self.deferred.is_none(),
            "drain transaction continuations before accepting more input"
        );
        let mut guard = self.guard();
        let result = if self.pending.is_empty() {
            self.process(input).await
        } else {
            self.deferred = Some(input);
            self.continue_pending().await
        };
        guard.complete = Self::completed(&result);
        result
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.check_running()?;
        let mut guard = self.guard();
        self.store.confirm(outputs).await?;
        guard.complete = true;
        Ok(())
    }
}

/// A graph-owned processing transaction: either an explicit linear sequence or
/// the shared query evaluator with its indexes, projections and scheduled work.
/// Both use the same core transaction owner; neither runs a nested query runtime.
pub struct TransactionTransformer {
    body: TransactionBody,
}

enum TransactionBody {
    Linear(Box<LinearTransaction>),
    Query(Box<ContinuousQueryTransformer>),
}

impl TransactionTransformer {
    pub async fn new(
        definition: TransactionTransformerDefinition,
        registry: Arc<TransactionalTransformerRegistry>,
        provider: Arc<dyn ComputationIndexProvider>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            body: TransactionBody::Linear(Box::new(
                LinearTransaction::new(definition, registry, provider).await?,
            )),
        })
    }

    async fn new_scoped(
        definition: TransactionTransformerDefinition,
        registry: Arc<TransactionalTransformerRegistry>,
        provider: Arc<dyn ComputationIndexProvider>,
        scope: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            body: TransactionBody::Linear(Box::new(
                LinearTransaction::new_scoped(definition, registry, provider, scope).await?,
            )),
        })
    }

    pub async fn query(
        definition: ContinuousQueryDefinition,
        provider: Arc<dyn ComputationIndexProvider>,
        options: QueryOptions,
        execution: QueryExecutionSettings,
        middleware: Option<Arc<MiddlewareTypeRegistry>>,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_query(
            ContinuousQueryTransformer::new_configured(
                definition, provider, options, execution, middleware,
            )
            .await?,
        ))
    }

    pub fn from_query(query: ContinuousQueryTransformer) -> Self {
        Self {
            body: TransactionBody::Query(Box::new(query.track_delivery())),
        }
    }

    pub fn query_results(&self) -> Option<QueryResults> {
        match &self.body {
            TransactionBody::Query(query) => Some(query.results()),
            TransactionBody::Linear(_) => None,
        }
    }

    pub fn with_scheduling(self, scheduling: Arc<QuerySchedulingResource>) -> anyhow::Result<Self> {
        match self.body {
            TransactionBody::Query(query) => Ok(Self {
                body: TransactionBody::Query(Box::new(query.with_scheduling(scheduling)?)),
            }),
            TransactionBody::Linear(_) => {
                anyhow::bail!("linear transaction steps do not own scheduled query work")
            }
        }
    }

    pub fn step_descriptors(&self) -> Box<dyn Iterator<Item = &ComponentDescriptor> + '_> {
        match &self.body {
            TransactionBody::Linear(sequence) => Box::new(sequence.step_descriptors()),
            TransactionBody::Query(query) => Box::new(std::iter::once(query.descriptor())),
        }
    }

    pub fn with_source_progress(self, progress: Arc<QuerySourceProgress>) -> anyhow::Result<Self> {
        Ok(Self {
            body: match self.body {
                TransactionBody::Linear(sequence) => {
                    TransactionBody::Linear(Box::new(sequence.with_source_progress(progress)?))
                }
                TransactionBody::Query(query) => {
                    TransactionBody::Query(Box::new(query.with_source_progress(progress)?))
                }
            },
        })
    }

    fn processor(&self) -> &dyn Transformer {
        match &self.body {
            TransactionBody::Linear(sequence) => sequence.as_ref(),
            TransactionBody::Query(query) => query.as_ref(),
        }
    }
    fn processor_mut(&mut self) -> &mut dyn Transformer {
        match &mut self.body {
            TransactionBody::Linear(sequence) => sequence.as_mut(),
            TransactionBody::Query(query) => query.as_mut(),
        }
    }
}

#[async_trait]
impl ComputationComponent for TransactionTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.processor().descriptor()
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        self.processor().configuration()
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.processor_mut().bind_control(control);
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.processor().control_handler()
    }
    fn requires_readiness_confirmation(&self) -> bool {
        self.processor().requires_readiness_confirmation()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.processor_mut().start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.processor_mut().stop().await
    }
}

#[async_trait]
impl Transformer for TransactionTransformer {
    fn has_pending_emissions(&self) -> bool {
        self.processor().has_pending_emissions()
    }
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        self.processor().wakeup_source()
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.processor_mut().transform(input).await
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.processor_mut().on_wakeup().await
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.processor_mut().continue_transform().await
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.processor_mut().delivery_completed(outputs).await
    }
}

pub struct TransactionTransformerFactory {
    descriptor: FactoryDescriptor,
}

impl Default for TransactionTransformerFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")
                    .expect("implementation"),
                role: ComponentRole::Transformer,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("transaction"),
                        ConfigurationField {
                            value_type: ConfigurationType::Object,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([
                    (
                        Arc::from("transformers"),
                        ResourceRequirement::exactly_one::<TransactionalTransformerRegistryResource>(
                            ResourceRole::Component,
                        ),
                    ),
                    (
                        Arc::from("indexes"),
                        ResourceRequirement::exactly_one::<QueryIndexProviderResource>(
                            ResourceRole::IndexBackend,
                        ),
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

#[async_trait]
impl ComponentFactory for TransactionTransformerFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            spec.role == ComponentRole::Transformer && spec.completion.is_none(),
            "transaction container must be a transformer"
        );
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("transaction") {
            let definition: TransactionTransformerDefinition =
                serde_json::from_value(value.clone())?;
            anyhow::ensure!(
                &definition.id == spec.descriptor.id(),
                "transaction definition ID mismatch"
            );
            anyhow::ensure!(
                !definition.steps.is_empty(),
                "transaction sequence must not be empty"
            );
        }
        Ok(())
    }

    fn validate_scope(
        &self,
        graph_id: &str,
        spec: &ComponentSpecification,
        _: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("transaction") {
            let definition: TransactionTransformerDefinition =
                serde_json::from_value(value.clone())?;
            anyhow::ensure!(
                definition.graph_id == graph_id,
                "transaction definition belongs to another graph"
            );
            for id in spec.dependencies.get("transformers").into_iter().flatten() {
                if let Some(handle) = resources.get(id) {
                    let registry = handle.get::<TransactionalTransformerRegistryResource>()?;
                    anyhow::ensure!(
                        definition.descriptor(&registry.0)? == spec.descriptor,
                        "transaction descriptor does not match its steps"
                    );
                }
            }
        }
        for id in spec.dependencies.get("indexes").into_iter().flatten() {
            if let Some(handle) = resources.get(id) {
                anyhow::ensure!(
                    !handle.get::<QueryIndexProviderResource>()?.0.is_volatile(),
                    "transaction storage must be persistent and atomic"
                );
            }
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let create = async {
            let definition: TransactionTransformerDefinition = serde_json::from_value(
                context
                    .configuration()
                    .get("transaction")
                    .ok_or_else(|| anyhow::anyhow!("missing transaction definition"))?
                    .clone(),
            )?;
            anyhow::ensure!(
                definition.graph_id == context.graph_id.as_ref()
                    && definition.id == context.component_id,
                "transaction scope mismatch"
            );
            let registry = context
                .resources::<TransactionalTransformerRegistryResource>("transformers")?
                .pop()
                .ok_or_else(|| anyhow::anyhow!("missing transactional transformer registry"))?;
            let provider = context
                .resources::<QueryIndexProviderResource>("indexes")?
                .pop()
                .ok_or_else(|| anyhow::anyhow!("missing shared transaction storage"))?;
            let mut transformer = TransactionTransformer::new_scoped(
                definition,
                registry.0.clone(),
                provider.0.clone(),
                context.instance_id.to_string(),
            )
            .await?;
            anyhow::ensure!(
                transformer.descriptor() == &context.specification.descriptor,
                "resolved transaction descriptor mismatch"
            );
            if context
                .specification
                .dependencies
                .contains_key("source_progress")
            {
                let progress = context
                    .resources::<QuerySourceProgressResource>("source_progress")?
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("missing source progress"))?;
                transformer = transformer.with_source_progress(progress.0.clone())?;
            }
            Ok(ConstructedComponent::transformer(Box::new(transformer)))
        };
        create.await.map_err(ComponentCreationError::terminal)
    }
}

struct TransactionalMiddlewareFactory(Arc<MiddlewareTypeRegistry>);

impl TransactionalTransformerFactory for TransactionalMiddlewareFactory {
    fn implementation(&self) -> ImplementationIdentity {
        ImplementationIdentity::try_new("drasi/middleware-transformer", "1")
            .expect("implementation")
    }
    fn configuration_version(&self) -> u32 {
        1
    }
    fn create(
        &self,
        definition: &TransactionStepDefinition,
    ) -> anyhow::Result<Box<dyn TransactionalTransformer>> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Configuration {
            middleware: Vec<drasi_core::models::SourceMiddlewareConfig>,
            pipeline: Vec<String>,
        }
        let config: Configuration = serde_json::from_value(definition.configuration.clone())?;
        Ok(Box::new(MiddlewareTransformer::new(
            MiddlewareTransformerDefinition {
                id: definition.id.clone(),
                output_stream: StreamId::try_new(format!(
                    "transaction-step/{}/out",
                    definition.id
                ))?,
                middleware: config.middleware,
                pipeline: config.pipeline,
            },
            self.0.clone(),
        )?))
    }
}
