// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native bootstrap factories as graph bootstrap provider nodes.
//!
//! Every native bootstrap factory a plugin publishes is registered as a
//! [`ComponentRole::BootstrapProvider`] component factory. The node has one
//! bootstrap output, [`BOOTSTRAP_PROVIDER_OUTPUT_PORT`], connected to a
//! source's bootstrap input (the source relays its consumers' sessions) or
//! directly to a transformer or query as its exclusive local provider.
//!
//! Each session creates its own native provider for the requesting
//! component, `BootstrapRequest::origin`. A paired factory serves the source
//! that relays its sessions, `BootstrapRequest::consumer`: the source owns its
//! library's coordinator and its consumers' progress, so the node binds that
//! source's coordinator and the origin's actual progress owner when each
//! session opens. Any other factory declaring source progress receives the
//! origin's progress from the node's `source_progress` dependency, which lists
//! one progress resource per bootstrapping query.

use super::{NativeBootstrapFactory, NativeSourceBootstrapResource};
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    BootstrapProviderComponent, BootstrapProviderFactory, ComponentCreationError,
    ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole, ComponentSpecification,
    ConfigurationValue, ConstructedComponent, ConstructionContext, FactoryDescriptor,
    GraphChangeCodec, PipeRequirements, PortDescriptor, PortDirection, PortId, QuerySourceProgress,
    QuerySourceProgressResource, ResourceRequirement, ResourceRole, StreamId,
    BOOTSTRAP_PROVIDER_OUTPUT_PORT,
};
use std::{any::TypeId, collections::BTreeMap, sync::Arc};

pub const SOURCE_PROGRESS: &str = "source_progress";

/// A graph component factory for one native bootstrap factory.
pub struct NativeBootstrapNodeFactory {
    factory: Arc<NativeBootstrapFactory>,
    descriptor: FactoryDescriptor,
}

impl NativeBootstrapNodeFactory {
    pub fn new(factory: Arc<NativeBootstrapFactory>) -> Self {
        let metadata = factory.metadata();
        let mut dependencies = BTreeMap::new();
        if metadata.source_progress && !factory.requires_source_bootstrap() {
            dependencies.insert(
                Arc::from(SOURCE_PROGRESS),
                ResourceRequirement {
                    role: ResourceRole::Checkpoint,
                    minimum: 1,
                    maximum: None,
                    concrete_type: Some(TypeId::of::<QuerySourceProgressResource>()),
                },
            );
        }
        let descriptor = FactoryDescriptor {
            implementation: metadata.implementation.clone(),
            role: ComponentRole::BootstrapProvider,
            configuration_version: metadata.configuration_version,
            configuration: super::runtime_configuration_schema(&metadata.configuration),
            dependencies,
        };
        Self {
            factory,
            descriptor,
        }
    }

    pub fn bootstrap_factory(&self) -> &Arc<NativeBootstrapFactory> {
        &self.factory
    }

    /// The node's component descriptor: a single bootstrap output.
    pub fn component_descriptor(id: ComponentId) -> anyhow::Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::bootstrap(
                PortId::try_new(BOOTSTRAP_PROVIDER_OUTPUT_PORT)?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }
}

#[async_trait]
impl ComponentFactory for NativeBootstrapNodeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn describe(
        &self,
        _context: &drasi_lib::computation::v1::ConstructionContext,
    ) -> anyhow::Result<drasi_lib::computation::v1::ComponentDescription> {
        Ok(BootstrapProviderComponent::provider_description())
    }

    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        self.factory
            .metadata()
            .configuration
            .validate(&configuration)?;
        let configuration = configuration
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("bootstrap configuration must be an object"))?
            .iter()
            .map(|(name, value)| {
                (
                    Arc::from(name.as_str()),
                    ConfigurationValue::Literal(value.clone()),
                )
            })
            .collect();
        Ok(ComponentSpecification {
            descriptor: Self::component_descriptor(id)?,
            role: ComponentRole::BootstrapProvider,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: self.descriptor.configuration_version,
            configuration,
            dependencies: BTreeMap::new(),
        })
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.implementation == self.descriptor.implementation
                && specification.configuration_version == self.descriptor.configuration_version
                && specification.role == ComponentRole::BootstrapProvider
                && specification.completion.is_none(),
            "native bootstrap provider implementation, version or role mismatch"
        );
        anyhow::ensure!(
            specification
                .dependencies
                .keys()
                .all(|name| self.descriptor.dependencies.contains_key(name)),
            "native bootstrap provider declares an unsupported dependency"
        );
        for (name, requirement) in &self.descriptor.dependencies {
            let count = specification.dependencies.get(name).map_or(0, Vec::len);
            anyhow::ensure!(
                count >= requirement.minimum
                    && requirement.maximum.is_none_or(|maximum| count <= maximum),
                "native bootstrap provider requires {}{} '{name}' resource(s)",
                requirement.minimum,
                requirement
                    .maximum
                    .map_or_else(|| "+".to_string(), |maximum| format!("..{maximum}")),
            );
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        self.validate(&context.specification)
            .map_err(ComponentCreationError::terminal)?;
        let progress: BTreeMap<ComponentId, Arc<QuerySourceProgress>> =
            if self.descriptor.dependencies.contains_key(SOURCE_PROGRESS) {
                context
                    .resources::<QuerySourceProgressResource>(SOURCE_PROGRESS)
                    .map_err(ComponentCreationError::terminal)?
                    .into_iter()
                    .map(|resource| {
                        anyhow::ensure!(
                            resource.0.graph_id() == context.graph_id.as_ref(),
                            "bootstrap progress belongs to another graph"
                        );
                        Ok((resource.0.query_id().clone(), resource.0.clone()))
                    })
                    .collect::<anyhow::Result<_>>()
                    .map_err(ComponentCreationError::terminal)?
            } else {
                BTreeMap::new()
            };
        let paired = self.factory.requires_source_bootstrap();
        if paired {
            validate_paired_wiring(&context).map_err(ComponentCreationError::terminal)?;
        }
        let configuration = serde_json::to_value(context.configuration())
            .map_err(ComponentCreationError::terminal)?;
        let scope = drasi_computation_plugin_sdk::Scope {
            instance_id: context.instance_id.to_string(),
            graph_id: context.graph_id.to_string(),
            generation: context.generation.0,
        };
        let factory = self.factory.clone();
        let running = context.output_catalogue().clone();
        let requires_progress = self.factory.metadata().source_progress;
        let sessions: BootstrapProviderFactory = Arc::new(move |request| {
            let origin = request.origin().clone();
            if paired {
                let source = NativeSourceBootstrapResource::registered(
                    &scope.instance_id,
                    &scope.graph_id,
                    &request.consumer,
                    Some(&running),
                )
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "paired bootstrap provider must be connected to the bootstrap input of a \
                         coordination-capable native source; '{}' is not one",
                        request.consumer
                    )
                })?;
                let owner = source.owner_for(&origin).ok_or_else(|| {
                    anyhow::anyhow!(
                        "source '{}' has no progress for bootstrap consumer '{origin}'",
                        request.consumer
                    )
                })?;
                let provider = factory.create_paired_query_provider(
                    origin,
                    configuration.clone(),
                    scope.clone(),
                    owner,
                    source,
                )?;
                return Ok(
                    provider as Arc<dyn drasi_lib::computation::v1::ComputationBootstrapProvider>
                );
            }
            let progress = if requires_progress {
                Some(progress.get(&origin).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "bootstrap provider has no source progress for consumer '{origin}'"
                    )
                })?)
            } else {
                None
            };
            let provider = factory.create_query_provider(
                origin,
                configuration.clone(),
                scope.clone(),
                progress,
            )?;
            Ok(provider as Arc<dyn drasi_lib::computation::v1::ComputationBootstrapProvider>)
        });
        let component = BootstrapProviderComponent::with_descriptor(
            context.specification.descriptor.clone(),
            sessions,
        )
        .map_err(ComponentCreationError::terminal)?;
        Ok(ConstructedComponent::bootstrap_provider(Box::new(
            component,
        )))
    }
}

/// A paired provider serves the source on its bootstrap output: its configured
/// `source` and `stream` must name that source and one of its output streams.
/// Only the wiring already in the desired graph is checked; sessions resolve
/// the actual source coordinator when they arrive.
fn validate_paired_wiring(context: &ConstructionContext) -> anyhow::Result<()> {
    let configured = |name: &str| {
        context
            .configuration()
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let graph = context.output_catalogue().desired_graph();
    let targets = graph
        .edges
        .iter()
        .filter(|edge| edge.definition.from.component == context.component_id)
        .filter_map(|edge| {
            let target = &edge.definition.to.component;
            graph
                .nodes
                .iter()
                .find(|node| node.descriptor.id() == target)
                .map(|node| (target, &node.output_streams))
        });
    check_paired_wiring(
        &context.component_id,
        configured("source").as_deref(),
        configured("stream").as_deref(),
        targets,
    )
}

fn check_paired_wiring<'a>(
    provider: &ComponentId,
    source: Option<&str>,
    stream: Option<&str>,
    targets: impl IntoIterator<Item = (&'a ComponentId, &'a BTreeMap<PortId, StreamId>)>,
) -> anyhow::Result<()> {
    for (target, streams) in targets {
        if let Some(source) = source {
            anyhow::ensure!(
                source == target.as_str(),
                "paired bootstrap provider '{provider}' is configured for source '{source}' but is \
                 connected to '{target}'"
            );
        }
        if let Some(stream) = stream {
            anyhow::ensure!(
                streams.values().any(|bound| bound.as_str() == stream),
                "paired bootstrap provider '{provider}' is configured for stream '{stream}', which \
                 source '{target}' does not publish"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_wiring_must_match_the_connected_source_and_its_stream() {
        let (provider, source) = (ComponentId::generate(), ComponentId::generate());
        let streams = BTreeMap::from([(
            PortId::try_new("out").unwrap(),
            StreamId::try_new("sqlite/out").unwrap(),
        )]);
        let name = source.to_string();
        let check = |configured_source: Option<&str>, stream: Option<&str>| {
            check_paired_wiring(&provider, configured_source, stream, [(&source, &streams)])
        };
        check(Some(&name), Some("sqlite/out")).unwrap();
        check(None, None).unwrap();
        let error = check(Some(&name), Some("other/out"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not publish"), "{error}");
        let other = ComponentId::generate().to_string();
        let error = check(Some(&other), None).unwrap_err().to_string();
        assert!(error.contains("but is connected to"), "{error}");
    }
}
