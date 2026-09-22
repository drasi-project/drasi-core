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

//! An ordered SourceChange middleware pipeline without a continuous query.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Context;
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::ElementIndex,
    middleware::{MiddlewareContainer, MiddlewareTypeRegistry, SourceMiddlewarePipeline},
    models::{SourceChange, SourceMiddlewareConfig},
};
use tokio::sync::Notify;

use super::middleware_recovery::MiddlewareStore;
use super::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole,
    ComponentSpecification, ComputationComponent, ConfigurationField, ConfigurationSchema,
    ConfigurationType, ConfigurationValue, ConstructedComponent, ConstructionContext,
    DurableMiddlewareOptions, FactoryDescriptor, GraphChangeCodec, GraphProducerIdentity,
    GraphProducerProgress, ImplementationIdentity, InputEnvelope, MiddlewareRecoveryError,
    MiddlewareRegistryResource, OutputEnvelope, PipeCapability, PipeRequirements, PortDescriptor,
    PortDirection, PortId, QueryIndexProviderResource, QuerySourceProgress,
    QuerySourceProgressResource, ResourceHandle, ResourceId, ResourceRequirement, ResourceRole,
    ResourceSpecification, StreamId, Transformer, WakeupSource,
};

const IMPLEMENTATION: &str = "drasi/middleware-transformer";

pub(super) fn validate_middleware_configs<'a>(
    configs: &'a [SourceMiddlewareConfig],
    registry: Option<&MiddlewareTypeRegistry>,
) -> anyhow::Result<BTreeSet<&'a str>> {
    let mut names = BTreeSet::new();
    for config in configs {
        super::data::validate_identifier("middleware", &config.name)?;
        super::data::validate_identifier("middleware kind", &config.kind)?;
        anyhow::ensure!(
            names.insert(config.name.as_ref()),
            "duplicate middleware name"
        );
        if registry.is_some_and(|registry| registry.get(&config.kind).is_none()) {
            anyhow::bail!("middleware kind {} is not registered", config.kind);
        }
    }
    Ok(names)
}

fn validate_pipeline(names: &BTreeSet<&str>, pipeline: &[String]) -> anyhow::Result<()> {
    for name in pipeline {
        super::data::validate_identifier("middleware pipeline step", name)?;
        anyhow::ensure!(
            names.contains(name.as_str()),
            "pipeline references undeclared middleware {name}"
        );
    }
    Ok(())
}

/// Middleware definitions and their execution order, using the query middleware
/// configuration format. All connected producers use the same pipeline.
#[derive(Debug, Clone)]
pub struct MiddlewareTransformerDefinition {
    pub id: ComponentId,
    pub output_stream: StreamId,
    pub middleware: Vec<SourceMiddlewareConfig>,
    pub pipeline: Vec<String>,
}

impl MiddlewareTransformerDefinition {
    /// One graph-change input and output. Multiple producers may connect to `in`.
    pub fn descriptor(&self) -> ComponentDescriptor {
        descriptor(self.id.clone(), false)
    }

    /// Durable mode requires FIFO/backpressure input and every outgoing branch
    /// to retain accepted output without loss until explicit consumer acknowledgement.
    pub fn durable_descriptor(&self) -> ComponentDescriptor {
        descriptor(self.id.clone(), true)
    }

    /// Declare this transformer using a supplied middleware registry resource.
    /// Bind the `out` port to `output_stream` when connecting the graph.
    pub fn specification(&self, registry: ResourceId) -> anyhow::Result<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: self.descriptor(),
            role: ComponentRole::Transformer,
            completion: None,
            implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")?,
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    Arc::from("stream"),
                    ConfigurationValue::Literal(serde_json::json!(self.output_stream.as_str())),
                ),
                (
                    Arc::from("middleware"),
                    ConfigurationValue::Literal(serde_json::to_value(&self.middleware)?),
                ),
                (
                    Arc::from("pipeline"),
                    ConfigurationValue::Literal(serde_json::to_value(&self.pipeline)?),
                ),
            ]),
            dependencies: BTreeMap::from([(Arc::from("middleware"), vec![registry])]),
        })
    }

    /// Uses `QueryIndexProviderResource` for the actual persistent index provider.
    /// An optional `source_progress` dependency may name a
    /// `QuerySourceProgressResource` owned by this middleware component.
    pub fn durable_specification(
        &self,
        registry: ResourceId,
        indexes: ResourceId,
        options: DurableMiddlewareOptions,
    ) -> anyhow::Result<ComponentSpecification> {
        super::data::validate_identifier("middleware graph", &options.graph_id)?;
        let mut spec = self.specification(registry)?;
        spec.descriptor = self.durable_descriptor();
        spec.configuration.insert(
            Arc::from("durability"),
            ConfigurationValue::Literal(serde_json::to_value(options)?),
        );
        spec.dependencies
            .insert(Arc::from("indexes"), vec![indexes]);
        Ok(spec)
    }
}

fn descriptor(id: ComponentId, durable: bool) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![
            PortDescriptor::new(
                PortId::try_new("in").expect("constant port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                if durable {
                    PipeRequirements::new([
                        PipeCapability::FifoPerStream,
                        PipeCapability::Backpressure,
                    ])
                } else {
                    PipeRequirements::default()
                },
            ),
            PortDescriptor::new(
                PortId::try_new("out").expect("constant port"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                if durable {
                    PipeRequirements::new([
                        PipeCapability::DurableAcceptance,
                        PipeCapability::Replay,
                        PipeCapability::ExplicitAcknowledgement,
                        PipeCapability::Backpressure,
                        PipeCapability::FifoPerStream,
                    ])
                } else {
                    PipeRequirements::default()
                },
            ),
        ],
    )
    .expect("middleware transformer descriptor")
}

struct ProcessingGuard {
    failed: Arc<AtomicBool>,
    progress: Option<Arc<QuerySourceProgress>>,
    complete: bool,
}

impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.failed.store(true, Ordering::Release);
            if let Some(progress) = &self.progress {
                progress.fail();
            }
        }
    }
}

#[derive(Default)]
struct PendingWakeup {
    pending: AtomicBool,
    notify: Notify,
}

impl PendingWakeup {
    fn set(&self, pending: bool) {
        self.pending.store(pending, Ordering::Release);
        self.notify.notify_waiters();
    }
}

#[async_trait]
impl WakeupSource for PendingWakeup {
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

pub(super) async fn remember(
    elements: &dyn ElementIndex,
    changes: &[SourceChange],
) -> anyhow::Result<()> {
    let slots = Vec::new();
    for change in changes {
        match change {
            SourceChange::Insert { element } => {
                elements.set_element(element, &slots).await?;
            }
            SourceChange::Update { element } => {
                let mut merged = element.clone();
                if let Some(previous) = elements.get_element(element.get_reference()).await? {
                    anyhow::ensure!(
                        std::mem::discriminant(&merged) == std::mem::discriminant(previous.as_ref()),
                        "middleware update cannot change an existing node into a relationship or vice versa"
                    );
                    merged.merge_missing_properties(previous.as_ref());
                }
                elements.set_element(&merged, &slots).await?;
            }
            SourceChange::Delete { metadata } => {
                elements.delete_element(&metadata.reference).await?;
            }
            SourceChange::Future { .. } => {}
        }
    }
    Ok(())
}

/// Reuses the continuous-query middleware pipeline for graph-change events.
///
/// Each new input produces one logical output batch, including an empty batch
/// when filtering removes every change. This preserves downstream progress.
/// Expansion stays in that batch, avoiding repeated raw source sequences across
/// separate outputs. Context, timestamp, source position and input lineage remain
/// intact; the transformer assigns its own output stream and sequence.
///
/// [`Self::new`] is explicitly volatile. [`Self::new_durable`] atomically persists
/// previous emitted elements, committed input progress, and exact transformed
/// output. Direct callers must drain continuations and call
/// [`Transformer::delivery_completed`] only after all branches accept durably.
/// Recovery emissions precede live input. Retried input replays retained output
/// without rerunning middleware; already-confirmed, evicted input needs no new
/// emission because the durable outgoing transports own its redelivery.
///
/// Durability covers the supplied index and emitted changes, not hidden mutable
/// state or external side effects inside a custom middleware implementation.
pub struct MiddlewareTransformer {
    definition: MiddlewareTransformerDefinition,
    descriptor: ComponentDescriptor,
    pipeline: SourceMiddlewarePipeline,
    elements: Arc<dyn ElementIndex>,
    identity: GraphProducerIdentity,
    durable: Option<MiddlewareStore>,
    source_progress: Option<Arc<QuerySourceProgress>>,
    pending: VecDeque<u64>,
    deferred: Option<InputEnvelope>,
    wakeup: Arc<PendingWakeup>,
    output_port: PortId,
    sequence: u64,
    running: bool,
    failed: Arc<AtomicBool>,
}

impl MiddlewareTransformer {
    pub fn new(
        definition: MiddlewareTransformerDefinition,
        registry: Arc<MiddlewareTypeRegistry>,
    ) -> anyhow::Result<Self> {
        let names = validate_middleware_configs(&definition.middleware, Some(&registry))?;
        validate_pipeline(&names, &definition.pipeline)?;
        let container = MiddlewareContainer::new(
            &registry,
            definition
                .middleware
                .iter()
                .cloned()
                .map(Arc::new)
                .collect(),
        )?;
        let pipeline = SourceMiddlewarePipeline::new(
            &container,
            definition
                .pipeline
                .iter()
                .map(|name| Arc::from(name.as_str()))
                .collect(),
        )?;
        Ok(Self {
            descriptor: definition.descriptor(),
            identity: GraphProducerIdentity::new(
                "volatile".into(),
                "volatile".into(),
                definition.id.clone(),
                definition.output_stream.clone(),
                false,
            )?,
            definition,
            pipeline,
            elements: Arc::new(InMemoryElementIndex::new()),
            durable: None,
            source_progress: None,
            pending: VecDeque::new(),
            deferred: None,
            wakeup: Arc::new(PendingWakeup::default()),
            output_port: PortId::try_new("out")?,
            sequence: 0,
            running: false,
            failed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Opens actual persistent resources; `start` validates/reloads their state
    /// before admitting input. Memory, incomplete, or non-atomic bundles fail.
    pub async fn new_durable(
        definition: MiddlewareTransformerDefinition,
        registry: Arc<MiddlewareTypeRegistry>,
        provider: Arc<dyn ComputationIndexProvider>,
        options: DurableMiddlewareOptions,
    ) -> anyhow::Result<Self> {
        let scope = options.graph_id.clone();
        Self::new_durable_scoped(definition, registry, provider, options, scope).await
    }

    async fn new_durable_scoped(
        definition: MiddlewareTransformerDefinition,
        registry: Arc<MiddlewareTypeRegistry>,
        provider: Arc<dyn ComputationIndexProvider>,
        options: DurableMiddlewareOptions,
        scope: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !provider.is_volatile(),
            "durable middleware rejects volatile providers"
        );
        super::data::validate_identifier("middleware graph", &options.graph_id)?;
        let mut transformer = Self::new(definition, registry)?;
        let indexes = provider
            .create_indexes(&options.graph_id, transformer.definition.id.as_str())
            .await?;
        let store = MiddlewareStore::new(indexes, &transformer.definition, options, scope)?;
        transformer.elements = store.elements();
        transformer.descriptor = transformer.definition.durable_descriptor();
        transformer.durable = Some(store);
        Ok(transformer)
    }

    /// Bind replay-capable, streaming source adapters (`enable_bootstrap=false`)
    /// to this middleware's committed recovery boundary, not a downstream query's
    /// unrelated logical cursor. Query bootstrap/reset is not a middleware bootstrap.
    pub fn with_source_progress(
        mut self,
        progress: Arc<QuerySourceProgress>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !self.running,
            "bind source progress before starting middleware"
        );
        let durable = self.durable.as_ref().ok_or_else(|| {
            anyhow::anyhow!("source recovery progress requires durable middleware")
        })?;
        anyhow::ensure!(
            progress.graph_id() == durable.graph_id() && progress.query_id() == &self.definition.id,
            "source progress belongs to another graph/component"
        );
        progress.require_stream_replay();
        progress.pending();
        self.source_progress = Some(progress);
        Ok(self)
    }

    fn publish_progress(&self) -> anyhow::Result<()> {
        if let (Some(progress), Some(durable)) = (&self.source_progress, &self.durable) {
            progress.publish(durable.progress()?);
        }
        Ok(())
    }

    fn guard(&self) -> ProcessingGuard {
        ProcessingGuard {
            failed: self.failed.clone(),
            progress: self.source_progress.clone(),
            complete: false,
        }
    }

    fn check_running(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.running, "middleware transformer is not running");
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "middleware transformer requires reconstruction before processing"
        );
        Ok(())
    }

    fn outputs(&self, envelope: super::ChangeEnvelope) -> Vec<OutputEnvelope> {
        vec![OutputEnvelope {
            port: self.output_port.clone(),
            envelope,
        }]
    }

    async fn continue_pending(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if let Some(sequence) = self.pending.front().copied() {
            let output = self
                .durable
                .as_mut()
                .expect("durable replay")
                .replay(sequence)
                .await?;
            self.pending.pop_front();
            self.wakeup
                .set(!self.pending.is_empty() || self.deferred.is_some());
            return Ok(self.outputs(output));
        }
        if let Some(input) = self.deferred.take() {
            self.wakeup.set(false);
            return self.process(input).await;
        }
        self.wakeup.set(false);
        Ok(Vec::new())
    }

    async fn process(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(input.port.as_str() == "in", "unknown middleware input port");
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        if let Some(durable) = &mut self.durable {
            let output = durable
                .process(&input.envelope, changes, &self.pipeline)
                .await?;
            self.publish_progress()?;
            return Ok(output.map_or_else(Vec::new, |envelope| self.outputs(envelope)));
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("middleware output sequence exhausted"))?;
        let mut output = Vec::new();
        for change in changes {
            let transformed = self
                .pipeline
                .process(change, self.elements.clone())
                .await
                .with_context(|| format!("middleware pipeline failed in {}", self.definition.id))?;
            remember(self.elements.as_ref(), &transformed).await?;
            output.extend(transformed);
        }
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &output,
            self.definition.output_stream.clone(),
            sequence,
        )?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        self.sequence = sequence;
        Ok(self.outputs(envelope))
    }
}

#[async_trait]
impl ComputationComponent for MiddlewareTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "middleware transformer must be reconstructed after failed or cancelled processing"
        );
        if self.running {
            return Ok(());
        }
        let mut guard = self.guard();
        if let Some(progress) = &self.source_progress {
            progress.pending();
        }
        if let Some(durable) = &mut self.durable {
            durable.recover().await?;
            self.pending = durable.pending()?;
            self.deferred = None;
            self.wakeup.set(!self.pending.is_empty());
            self.publish_progress()?;
        }
        self.running = true;
        guard.complete = true;
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        self.wakeup.set(false);
        if let Some(durable) = &self.durable {
            if self.failed.load(Ordering::Acquire) || durable.transaction.recovery_required() {
                if let Some(progress) = &self.source_progress {
                    progress.fail();
                }
                durable.transaction.shutdown().await?;
            } else if let Err(error) = durable.transaction.quiesce().await {
                self.failed.store(true, Ordering::Release);
                if let Some(progress) = &self.source_progress {
                    progress.fail();
                }
                return Err(error.into());
            } else if let Some(progress) = &self.source_progress {
                progress.pending();
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Transformer for MiddlewareTransformer {
    fn has_pending_emissions(&self) -> bool {
        !self.pending.is_empty() || self.deferred.is_some()
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        self.durable
            .as_ref()
            .map(|_| self.wakeup.clone() as Arc<dyn WakeupSource>)
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.continue_transform().await
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        let mut guard = self.guard();
        let result = self.continue_pending().await;
        guard.complete = retryable_result(&result);
        result
    }

    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.check_running()?;
        let mut guard = self.guard();
        if let Some(durable) = &mut self.durable {
            durable.confirm(outputs).await?;
        }
        guard.complete = true;
        Ok(())
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        anyhow::ensure!(
            self.deferred.is_none(),
            "drain middleware continuations before more input"
        );
        let mut guard = self.guard();
        let result = if self.pending.is_empty() {
            self.process(input).await
        } else {
            self.deferred = Some(input);
            self.continue_pending().await
        };
        guard.complete = retryable_result(&result);
        result
    }
}

fn retryable_result(result: &anyhow::Result<Vec<OutputEnvelope>>) -> bool {
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

/// Builds a MiddlewareTransformer from `stream`, `middleware` and `pipeline`
/// configuration fields and one `MiddlewareRegistryResource` dependency.
/// `durability` additionally requires persistent `indexes`; `source_progress`
/// optionally exposes the middleware's own committed input boundary.
pub struct MiddlewareTransformerFactory {
    descriptor: FactoryDescriptor,
}

impl Default for MiddlewareTransformerFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")
                    .expect("middleware implementation"),
                role: ComponentRole::Transformer,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: [
                        ("stream", ConfigurationType::String, true),
                        ("middleware", ConfigurationType::Array, true),
                        ("pipeline", ConfigurationType::Array, true),
                        ("durability", ConfigurationType::Object, false),
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
                    .collect(),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([
                    (
                        Arc::from("middleware"),
                        ResourceRequirement::exactly_one::<MiddlewareRegistryResource>(
                            ResourceRole::Middleware,
                        ),
                    ),
                    (
                        Arc::from("indexes"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QueryIndexProviderResource>(
                                ResourceRole::IndexBackend,
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

fn literal<'a>(spec: &'a ComponentSpecification, name: &str) -> Option<&'a serde_json::Value> {
    match spec.configuration.get(name) {
        Some(ConfigurationValue::Literal(value)) => Some(value),
        _ => None,
    }
}

#[async_trait]
impl ComponentFactory for MiddlewareTransformerFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let durable = spec.configuration.contains_key("durability");
        anyhow::ensure!(
            spec.role == ComponentRole::Transformer
                && spec.completion.is_none()
                && spec.descriptor == descriptor(spec.descriptor.id().clone(), durable),
            "middleware transformer requires graph-change input and output ports"
        );
        let indexes = spec.dependencies.get("indexes").map_or(0, Vec::len);
        anyhow::ensure!(
            if durable { indexes == 1 } else { indexes == 0 },
            "durable middleware requires exactly one index provider; volatile mode takes none"
        );
        anyhow::ensure!(
            durable
                || spec
                    .dependencies
                    .get("source_progress")
                    .map_or(true, Vec::is_empty),
            "volatile middleware cannot declare persistent source progress"
        );
        if let Some(options) = literal(spec, "durability") {
            let options: DurableMiddlewareOptions = serde_json::from_value(options.clone())?;
            super::data::validate_identifier("middleware graph", &options.graph_id)?;
        }
        if let Some(stream) = literal(spec, "stream") {
            StreamId::try_new(
                stream
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid middleware output stream"))?,
            )?;
        }
        let configs = literal(spec, "middleware")
            .map(|value| serde_json::from_value::<Vec<SourceMiddlewareConfig>>(value.clone()))
            .transpose()?;
        let pipeline = literal(spec, "pipeline")
            .map(|value| serde_json::from_value::<Vec<String>>(value.clone()))
            .transpose()?;
        if let Some(pipeline) = &pipeline {
            for name in pipeline {
                super::data::validate_identifier("middleware pipeline step", name)?;
            }
        }
        if let Some(configs) = &configs {
            let names = validate_middleware_configs(configs, None)?;
            if let Some(pipeline) = &pipeline {
                validate_pipeline(&names, pipeline)?;
            }
        }
        Ok(())
    }

    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        _declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        for id in spec.dependencies.get("indexes").into_iter().flatten() {
            if let Some(handle) = resources.get(id) {
                anyhow::ensure!(
                    !handle.get::<QueryIndexProviderResource>()?.0.is_volatile(),
                    "durable middleware rejects volatile providers"
                );
            }
        }
        if let Some(value) = literal(spec, "middleware") {
            let configs: Vec<SourceMiddlewareConfig> = serde_json::from_value(value.clone())?;
            for id in spec.dependencies.get("middleware").into_iter().flatten() {
                if let Some(resource) = resources.get(id) {
                    let registry = resource.get::<MiddlewareRegistryResource>()?;
                    validate_middleware_configs(&configs, Some(&registry.0))?;
                }
            }
        }
        Ok(())
    }

    fn validate_scope(
        &self,
        graph_id: &str,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate_resources(spec, declarations, resources)?;
        if let Some(options) = literal(spec, "durability") {
            let options: DurableMiddlewareOptions = serde_json::from_value(options.clone())?;
            anyhow::ensure!(
                options.graph_id == graph_id,
                "middleware durability belongs to another graph"
            );
        }
        for id in spec
            .dependencies
            .get("source_progress")
            .into_iter()
            .flatten()
        {
            if let Some(handle) = resources.get(id) {
                let progress = handle.get::<QuerySourceProgressResource>()?;
                anyhow::ensure!(
                    progress.0.graph_id() == graph_id
                        && progress.0.query_id() == spec.descriptor.id(),
                    "source progress belongs to another graph/component"
                );
            }
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let registry = context
            .resources::<MiddlewareRegistryResource>("middleware")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing middleware registry"))
            })?;
        let configuration = context.configuration();
        let value = |name: &str| {
            configuration.get(name).ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!(
                    "missing middleware configuration field {name}"
                ))
            })
        };
        let stream = value("stream")?.as_str().ok_or_else(|| {
            ComponentCreationError::terminal(anyhow::anyhow!("invalid middleware output stream"))
        })?;
        let definition = MiddlewareTransformerDefinition {
            id: context.component_id.clone(),
            output_stream: StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?,
            middleware: serde_json::from_value(value("middleware")?.clone())
                .map_err(ComponentCreationError::terminal)?,
            pipeline: serde_json::from_value(value("pipeline")?.clone())
                .map_err(ComponentCreationError::terminal)?,
        };
        let mut transformer = if let Some(options) = configuration.get("durability") {
            let options: DurableMiddlewareOptions = serde_json::from_value(options.clone())
                .map_err(ComponentCreationError::terminal)?;
            if options.graph_id != context.graph_id.as_ref() {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "middleware durability belongs to another graph"
                )));
            }
            let provider = context
                .resources::<QueryIndexProviderResource>("indexes")
                .map_err(ComponentCreationError::terminal)?
                .pop()
                .ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "missing durable middleware indexes"
                    ))
                })?;
            MiddlewareTransformer::new_durable_scoped(
                definition,
                registry.0.clone(),
                provider.0.clone(),
                options,
                context.instance_id.to_string(),
            )
            .await
            .map_err(ComponentCreationError::terminal)?
        } else {
            MiddlewareTransformer::new(definition, registry.0.clone())
                .map_err(ComponentCreationError::terminal)?
        };
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
                transformer = transformer
                    .with_source_progress(progress.0.clone())
                    .map_err(ComponentCreationError::terminal)?;
            }
        }
        Ok(ConstructedComponent::transformer(Box::new(transformer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::{
        interface::{
            MiddlewareError, MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory,
        },
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
    };
    use tokio::sync::Notify;

    fn definition() -> MiddlewareTransformerDefinition {
        MiddlewareTransformerDefinition {
            id: ComponentId::try_new("middleware").expect("id"),
            output_stream: StreamId::try_new("middleware/out").expect("stream"),
            middleware: Vec::new(),
            pipeline: Vec::new(),
        }
    }

    fn input(sequence: u64) -> InputEnvelope {
        InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: GraphChangeCodec::encode_change(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("source", &sequence.to_string()),
                            labels: Arc::from([Arc::from("Item")]),
                            effective_from: sequence,
                        },
                        properties: ElementPropertyMap::from(serde_json::json!({"value":sequence})),
                    },
                },
                StreamId::try_new("source/out").expect("stream"),
                sequence,
                None,
            )
            .expect("input"),
        }
    }

    struct WaitingMiddleware(Arc<Notify>);

    impl SourceMiddlewareFactory for WaitingMiddleware {
        fn name(&self) -> String {
            "waiting".into()
        }

        fn create(
            &self,
            _: &SourceMiddlewareConfig,
        ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
            Ok(Arc::new(Self(self.0.clone())))
        }
    }

    #[async_trait]
    impl SourceMiddleware for WaitingMiddleware {
        async fn process(
            &self,
            _: SourceChange,
            _: &dyn ElementIndex,
        ) -> Result<Vec<SourceChange>, MiddlewareError> {
            self.0.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancelled_processing_cannot_restart_with_potentially_partial_state() {
        let entered = Arc::new(Notify::new());
        let mut registry = MiddlewareTypeRegistry::new();
        registry.register(Arc::new(WaitingMiddleware(entered.clone())));
        let mut definition = definition();
        definition.middleware.push(SourceMiddlewareConfig::new(
            "waiting",
            "wait",
            serde_json::Map::new(),
        ));
        definition.pipeline.push("wait".into());
        let mut transformer = MiddlewareTransformer::new(definition.clone(), Arc::new(registry))
            .expect("transformer");
        transformer.start().await.expect("start");
        {
            let processing = transformer.transform(input(1));
            tokio::pin!(processing);
            tokio::select! {
                result = &mut processing => panic!("unexpected completion: {result:?}"),
                _ = entered.notified() => {}
            }
        }
        transformer.stop().await.expect("stop");
        assert!(transformer.start().await.is_err());
        assert_eq!(transformer.sequence, 0);
    }

    #[tokio::test]
    async fn sequence_exhaustion_fails_before_changing_element_state() {
        let mut transformer =
            MiddlewareTransformer::new(definition(), Arc::new(MiddlewareTypeRegistry::new()))
                .expect("transformer");
        transformer.sequence = u64::MAX;
        transformer.start().await.expect("start");
        assert!(transformer.transform(input(1)).await.is_err());
        assert_eq!(transformer.sequence, u64::MAX);
        assert!(transformer
            .elements
            .get_element(&ElementReference::new("source", "1"))
            .await
            .expect("lookup")
            .is_none());
        transformer.stop().await.expect("stop");
        assert!(transformer.start().await.is_err());
    }

    #[tokio::test]
    async fn stopping_preserves_state_and_the_output_sequence() {
        let mut transformer =
            MiddlewareTransformer::new(definition(), Arc::new(MiddlewareTypeRegistry::new()))
                .expect("transformer");
        assert!(transformer.transform(input(1)).await.is_err());
        transformer.start().await.expect("start");
        let first = transformer.transform(input(1)).await.expect("first");
        transformer.stop().await.expect("stop");
        assert!(transformer.transform(input(2)).await.is_err());
        transformer.start().await.expect("restart");
        let second = transformer.transform(input(2)).await.expect("second");
        assert_eq!(first[0].envelope.system().sequence(), 1);
        assert_eq!(second[0].envelope.system().sequence(), 2);
        assert!(transformer
            .elements
            .get_element(&ElementReference::new("source", "1"))
            .await
            .expect("retained state")
            .is_some());
    }
}
