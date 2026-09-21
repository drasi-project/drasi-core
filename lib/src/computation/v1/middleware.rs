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
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Context;
use async_trait::async_trait;
use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::ElementIndex,
    middleware::{MiddlewareContainer, MiddlewareTypeRegistry, SourceMiddlewarePipeline},
    models::{SourceChange, SourceMiddlewareConfig},
};

use super::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole,
    ComponentSpecification, ComputationComponent, ConfigurationField, ConfigurationSchema,
    ConfigurationType, ConfigurationValue, ConstructedComponent, ConstructionContext,
    FactoryDescriptor, GraphChangeCodec, ImplementationIdentity, InputEnvelope,
    MiddlewareRegistryResource, OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection,
    PortId, ResourceHandle, ResourceId, ResourceRequirement, ResourceRole, ResourceSpecification,
    StreamId, Transformer,
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
        descriptor(self.id.clone())
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
}

fn descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![
            PortDescriptor::new(
                PortId::try_new("in").expect("constant port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ),
            PortDescriptor::new(
                PortId::try_new("out").expect("constant port"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ),
        ],
    )
    .expect("middleware transformer descriptor")
}

struct ProcessingGuard {
    failed: Arc<AtomicBool>,
    complete: bool,
}

impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.failed.store(true, Ordering::Release);
        }
    }
}

/// Reuses the continuous-query middleware pipeline for graph-change events.
///
/// Each input produces one output batch, including an empty batch when filtering
/// removes every change. This preserves source progress for downstream queries.
/// Expansion stays in that batch, avoiding repeated raw source sequences across
/// separate outputs. Context, timestamp, source position and input lineage remain
/// intact; the transformer assigns its own output stream and sequence.
///
/// Previous emitted elements are held in memory for middleware such as Unwind.
/// Successful stop/start retains this state. Failed or cancelled processing
/// requires reconstruction; this component does not provide durable recovery.
pub struct MiddlewareTransformer {
    definition: MiddlewareTransformerDefinition,
    descriptor: ComponentDescriptor,
    pipeline: SourceMiddlewarePipeline,
    elements: Arc<InMemoryElementIndex>,
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
            definition,
            pipeline,
            elements: Arc::new(InMemoryElementIndex::new()),
            output_port: PortId::try_new("out")?,
            sequence: 0,
            running: false,
            failed: Arc::new(AtomicBool::new(false)),
        })
    }

    async fn remember(&self, changes: &[SourceChange]) -> anyhow::Result<()> {
        let slots = Vec::new();
        for change in changes {
            match change {
                SourceChange::Insert { element } => {
                    self.elements.set_element(element, &slots).await?;
                }
                SourceChange::Update { element } => {
                    let mut merged = element.clone();
                    if let Some(previous) =
                        self.elements.get_element(element.get_reference()).await?
                    {
                        anyhow::ensure!(
                            std::mem::discriminant(&merged)
                                == std::mem::discriminant(previous.as_ref()),
                            "middleware update cannot change an existing node into a relationship or vice versa"
                        );
                        merged.merge_missing_properties(previous.as_ref());
                    }
                    self.elements.set_element(&merged, &slots).await?;
                }
                SourceChange::Delete { metadata } => {
                    self.elements.delete_element(&metadata.reference).await?;
                }
                SourceChange::Future { .. } => {}
            }
        }
        Ok(())
    }

    async fn process(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(input.port.as_str() == "in", "unknown middleware input port");
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
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
            self.remember(&transformed).await?;
            output.extend(transformed);
        }
        let envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &output,
            self.definition.output_stream.clone(),
            sequence,
        )?;
        self.sequence = sequence;
        Ok(vec![OutputEnvelope {
            port: self.output_port.clone(),
            envelope,
        }])
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
        self.running = true;
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        Ok(())
    }
}

#[async_trait]
impl Transformer for MiddlewareTransformer {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(self.running, "middleware transformer is not running");
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "middleware transformer requires reconstruction before processing"
        );
        let mut guard = ProcessingGuard {
            failed: self.failed.clone(),
            complete: false,
        };
        let result = self.process(input).await;
        guard.complete = result.is_ok();
        result
    }
}

/// Builds a MiddlewareTransformer from `stream`, `middleware` and `pipeline`
/// configuration fields and one `MiddlewareRegistryResource` dependency.
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
                        ("stream", ConfigurationType::String),
                        ("middleware", ConfigurationType::Array),
                        ("pipeline", ConfigurationType::Array),
                    ]
                    .into_iter()
                    .map(|(name, value_type)| {
                        (
                            Arc::from(name),
                            ConfigurationField {
                                value_type,
                                required: true,
                                secret: false,
                            },
                        )
                    })
                    .collect(),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([(
                    Arc::from("middleware"),
                    ResourceRequirement::exactly_one::<MiddlewareRegistryResource>(
                        ResourceRole::Middleware,
                    ),
                )]),
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
        anyhow::ensure!(
            spec.role == ComponentRole::Transformer
                && spec.completion.is_none()
                && spec.descriptor == descriptor(spec.descriptor.id().clone()),
            "middleware transformer requires graph-change input and output ports"
        );
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
        let transformer = MiddlewareTransformer::new(definition, registry.0.clone())
            .map_err(ComponentCreationError::terminal)?;
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
