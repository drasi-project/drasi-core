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

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::{
    BindingState, ComponentDescriptor, ComponentFailure, ComponentGeneration, ComponentId,
    ComponentRole, ConfigurationValue, DataAvailability, DesiredPipe, DesiredRelationship,
    EdgeDefinition, GraphRevision, GraphSnapshot, ImplementationIdentity, NodeSnapshot,
    ObservedComponent, ObservedGraph, ObservedRelationship, ObservedResource, PipeCapabilities,
    PipeProvider, PluginIdentity, PortId, ResourceId, ResourceRole, ResourceSpecification,
};

/// An entity identity scoped to one computation graph. Native pipes use full
/// port endpoints; host subscriptions use component pairs in a separate namespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphEntityId {
    Component(ComponentId),
    Resource(ResourceId),
    /// A particular plugin ID/version pair.
    Plugin(PluginIdentity),
    PluginFamily(Arc<str>),
    Pipe(EdgeDefinition),
    Subscription {
        from: ComponentId,
        to: ComponentId,
    },
}

pub(super) fn encode_entity_key(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

impl GraphEntityId {
    /// Stable, collision-free graph-as-data key. Each identifier is encoded
    /// separately, so delimiters in user identifiers cannot alias another entity.
    /// Component/resource keys retain the original inspection source namespaces.
    pub fn key(&self) -> String {
        match self {
            Self::Component(id) => format!("component:{}", encode_entity_key(id.as_str())),
            Self::Resource(id) => format!("resource:{}", encode_entity_key(id.as_str())),
            Self::Plugin(identity) => format!(
                "v1:plugin:{}:{}",
                encode_entity_key(&identity.id),
                encode_entity_key(&identity.version),
            ),
            Self::PluginFamily(id) => format!("v1:plugin-family:{}", encode_entity_key(id)),
            Self::Pipe(edge) => format!(
                "v1:pipe:{}:{}:{}:{}",
                encode_entity_key(edge.from.component.as_str()),
                encode_entity_key(edge.from.port.as_str()),
                encode_entity_key(edge.to.component.as_str()),
                encode_entity_key(edge.to.port.as_str()),
            ),
            Self::Subscription { from, to } => format!(
                "v1:subscription:{}:{}",
                encode_entity_key(from.as_str()),
                encode_entity_key(to.as_str()),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphEntityKind {
    Component,
    Resource,
    Plugin,
    PluginFamily,
    Pipe,
}

/// Known construction metadata, without configuration values or factory handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentEntityConstruction {
    Factory {
        implementation: ImplementationIdentity,
        configuration_version: u32,
    },
    /// A preconstructed instance still needs an external binding on import.
    /// Declaring plugin provenance does not make it reconstructible.
    External,
}

/// A component's semantic category, separate from its port/lifecycle execution
/// role. Hosts can annotate wrappers through [`ComponentDescriptor::with_semantic_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComponentSemanticKind {
    Source,
    Transformer,
    Query,
    Sink,
    Reaction,
    Service,
}

impl From<ComponentRole> for ComponentSemanticKind {
    fn from(role: ComponentRole) -> Self {
        match role {
            ComponentRole::Source => Self::Source,
            ComponentRole::Transformer => Self::Transformer,
            ComponentRole::Query => Self::Query,
            ComponentRole::Sink => Self::Sink,
            ComponentRole::Service => Self::Service,
        }
    }
}

impl ComponentSemanticKind {
    fn for_component(role: ComponentRole, descriptor: &ComponentDescriptor) -> Self {
        descriptor.semantic_kind().unwrap_or_else(|| role.into())
    }
}

#[derive(Debug, Clone)]
pub struct ComponentEntity {
    /// Includes the complete declared port/schema interfaces.
    pub desired: NodeSnapshot,
    /// Uses explicit descriptor metadata, falling back to the execution role.
    /// Implementation identity and configuration never determine this category.
    pub kind: ComponentSemanticKind,
    pub construction: ComponentEntityConstruction,
    /// Authoritative host-observed origin, separate from the immutable descriptor.
    pub host_plugin_identity: Option<PluginIdentity>,
    pub observed: Option<ObservedComponent>,
}

impl ComponentEntity {
    /// Prefer authoritative host observations, then explicit factory provenance,
    /// then the wrapped instance's descriptor. No identity is inferred from types.
    pub fn plugin_identity(&self) -> Option<&PluginIdentity> {
        self.host_plugin_identity
            .as_ref()
            .or_else(|| match &self.construction {
                ComponentEntityConstruction::Factory { implementation, .. } => implementation
                    .plugin
                    .as_ref()
                    .or_else(|| self.desired.descriptor.plugin_identity()),
                ComponentEntityConstruction::External => self.desired.descriptor.plugin_identity(),
            })
    }
}

/// A real graph resource declaration and its observation, never a synthetic
/// provider inferred from a component's Rust type or configuration values.
#[derive(Debug, Clone)]
pub struct ResourceEntity {
    pub desired: ResourceSpecification,
    pub observed: Option<ObservedResource>,
}

/// Derived from the current dependent components, not a plugin-loader inventory.
/// Different versions of the same plugin always occupy different nodes.
#[derive(Debug, Clone)]
pub struct PluginEntity {
    pub identity: PluginIdentity,
    pub dependent_components: BTreeSet<ComponentId>,
}

impl PluginEntity {
    pub fn dependent_component_count(&self) -> usize {
        self.dependent_components.len()
    }
}

/// The unversioned plugin identity shared by its currently represented versions.
/// Components are counted once across those versions, within this graph scope.
#[derive(Debug, Clone)]
pub struct PluginFamilyEntity {
    pub id: Arc<str>,
    pub versions: BTreeSet<PluginIdentity>,
    pub dependent_components: BTreeSet<ComponentId>,
}

impl PluginFamilyEntity {
    pub fn version_count(&self) -> usize {
        self.versions.len()
    }

    pub fn dependent_component_count(&self) -> usize {
        self.dependent_components.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PipeRepresentation {
    NativeProvider,
    HostSubscription,
}

#[derive(Debug, Clone)]
pub struct PipeEntity {
    pub relationship: DesiredRelationship,
    /// Negotiated capabilities of a configured edge, absent for an unbound declaration.
    pub capabilities: Option<PipeCapabilities>,
    pub resources: BTreeMap<ResourceId, ResourceRole>,
    /// A retained relationship with no configured edge. This does not describe
    /// provider binding success; consult `binding()` for the observed state.
    pub declared_only: bool,
    pub observed: Option<ObservedRelationship>,
}

impl PipeEntity {
    pub fn representation(&self) -> PipeRepresentation {
        PipeRepresentation::NativeProvider
    }

    pub fn binding(&self) -> BindingState {
        self.observed
            .as_ref()
            .map_or(BindingState::Declared, |state| state.binding)
    }

    pub fn availability(&self) -> DataAvailability {
        self.observed
            .as_ref()
            .map_or(DataAvailability::Unknown, |state| state.availability)
    }

    pub fn generation(&self) -> Option<u64> {
        self.observed.as_ref().map(|state| state.generation)
    }

    /// Retains the original local failure cause; graph-as-data emits only its phase.
    pub fn failure(&self) -> Option<&ComponentFailure> {
        self.observed
            .as_ref()
            .and_then(|state| state.failure.as_ref())
    }
}

/// An actual configured data subscription executed by an ordinary component host.
/// This is a pipe entity for topology, not a native `PipeProvider` instance.
/// No native ports, capacity, capabilities or binding state are inferred.
#[derive(Debug, Clone)]
pub struct SubscriptionPipeEntity {
    pub from: ComponentId,
    pub to: ComponentId,
    pub producer_observed: Option<ObservedComponent>,
    pub consumer_observed: Option<ObservedComponent>,
    /// Declared readiness gating, not confirmation of current peer readiness.
    pub producer_requires_downstream_ready: bool,
    pub consumer_requires_downstream_ready: bool,
}

impl SubscriptionPipeEntity {
    pub fn representation(&self) -> PipeRepresentation {
        PipeRepresentation::HostSubscription
    }

    pub fn producer_generation(&self) -> Option<ComponentGeneration> {
        self.producer_observed
            .as_ref()
            .map(|state| state.generation)
    }

    pub fn consumer_generation(&self) -> Option<ComponentGeneration> {
        self.consumer_observed
            .as_ref()
            .map(|state| state.generation)
    }

    /// Published startup state only. Current peer readiness is not in the snapshot.
    pub fn producer_started(&self) -> Option<bool> {
        self.producer_observed.as_ref().map(|state| state.started)
    }

    /// Published startup state only. Current peer readiness is not in the snapshot.
    pub fn consumer_started(&self) -> Option<bool> {
        self.consumer_observed.as_ref().map(|state| state.started)
    }
}

#[derive(Debug, Clone)]
pub enum GraphEntity {
    Component(ComponentEntity),
    Resource(ResourceEntity),
    Plugin(PluginEntity),
    PluginFamily(PluginFamilyEntity),
    Pipe(PipeEntity),
    SubscriptionPipe(SubscriptionPipeEntity),
}

impl GraphEntity {
    pub fn kind(&self) -> GraphEntityKind {
        match self {
            Self::Component(_) => GraphEntityKind::Component,
            Self::Resource(_) => GraphEntityKind::Resource,
            Self::Plugin(_) => GraphEntityKind::Plugin,
            Self::PluginFamily(_) => GraphEntityKind::PluginFamily,
            Self::Pipe(_) | Self::SubscriptionPipe(_) => GraphEntityKind::Pipe,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphEntityLinkKind {
    /// A declared, observed component-provider, configuration or pipe resource dependency.
    UsesResource,
    /// A host adapter refers to the actual component in an owning graph scope.
    UsesComponent,
    DependsOnPlugin,
    /// Plugin version -> unversioned plugin family, without lifecycle coupling.
    VersionOfPlugin,
    /// Consumer -> pipe -> producer. Descriptive data dependency only; this
    /// does not imply required creation, activation or failure coupling.
    DependsOnData,
    /// Host-declared control-only adjacency, not a data subscription or pipe.
    ControlConnection,
    /// Producer component -> pipe, naming the component's output port.
    PipeInput {
        port: PortId,
    },
    /// Pipe -> consumer component, naming the component's input port.
    PipeOutput {
        port: PortId,
    },
    /// Producer component -> host subscription pipe. No native port is declared.
    SubscriptionInput,
    /// Host subscription pipe -> consumer component. No native port is declared.
    SubscriptionOutput,
}

impl GraphEntityLinkKind {
    pub fn is_dependency(&self) -> bool {
        matches!(
            self,
            Self::UsesResource
                | Self::UsesComponent
                | Self::DependsOnPlugin
                | Self::VersionOfPlugin
                | Self::DependsOnData
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphEntityLink {
    pub from: GraphEntityId,
    pub to: GraphEntityId,
    pub kind: GraphEntityLinkKind,
}

/// A unified, descriptive view of one desired/observed publication. It contains
/// no live handles, resolved configuration, or inferred construction recipes.
/// Local observations retain their original failure references.
///
/// Declared connection links may reference absent components. Their identities
/// are retained rather than inventing placeholder components or pipe providers.
#[derive(Debug, Clone)]
pub struct ComputationTopology {
    pub graph_id: Arc<str>,
    pub revision: GraphRevision,
    pub run_epoch: u64,
    pub nodes: BTreeMap<GraphEntityId, GraphEntity>,
    pub links: BTreeSet<GraphEntityLink>,
}

impl ComputationTopology {
    pub fn dependencies<'a>(
        &'a self,
        dependent: &'a GraphEntityId,
    ) -> impl Iterator<Item = &'a GraphEntityLink> {
        self.links
            .iter()
            .filter(move |link| &link.from == dependent && link.kind.is_dependency())
    }

    pub fn dependents<'a>(
        &'a self,
        dependency: &'a GraphEntityId,
    ) -> impl Iterator<Item = &'a GraphEntityLink> {
        self.links
            .iter()
            .filter(move |link| &link.to == dependency && link.kind.is_dependency())
    }

    pub(crate) fn new(desired: &GraphSnapshot, observed: &ObservedGraph) -> Self {
        let mut topology = Self {
            graph_id: desired.id.clone(),
            revision: desired.revision,
            run_epoch: observed.run_epoch,
            nodes: BTreeMap::new(),
            links: BTreeSet::new(),
        };
        let mut plugins = BTreeMap::<PluginIdentity, BTreeSet<ComponentId>>::new();
        for node in desired.nodes.iter() {
            let id = node.descriptor.id();
            let entity_id = GraphEntityId::Component(id.clone());
            let specification = desired.specifications.get(id);
            let entity = ComponentEntity {
                desired: node.clone(),
                kind: ComponentSemanticKind::for_component(node.role, &node.descriptor),
                construction: specification.map_or(ComponentEntityConstruction::External, |spec| {
                    ComponentEntityConstruction::Factory {
                        implementation: spec.implementation.clone(),
                        configuration_version: spec.configuration_version,
                    }
                }),
                host_plugin_identity: desired.component_plugins.get(id).cloned(),
                observed: observed.components.get(id).cloned(),
            };
            if let Some(plugin) = entity.plugin_identity() {
                plugins
                    .entry(plugin.clone())
                    .or_default()
                    .insert(id.clone());
                topology.links.insert(GraphEntityLink {
                    from: entity_id.clone(),
                    to: GraphEntityId::Plugin(plugin.clone()),
                    kind: GraphEntityLinkKind::DependsOnPlugin,
                });
            }
            let resources = specification.into_iter().flat_map(|spec| {
                spec.dependencies
                    .values()
                    .flatten()
                    .chain(spec.configuration.values().filter_map(|value| {
                        if let ConfigurationValue::Reference { resource, .. } = value {
                            Some(resource)
                        } else {
                            None
                        }
                    }))
            });
            for resource in
                resources.chain(desired.component_resources.get(id).into_iter().flatten())
            {
                topology.links.insert(GraphEntityLink {
                    from: entity_id.clone(),
                    to: GraphEntityId::Resource(resource.clone()),
                    kind: GraphEntityLinkKind::UsesResource,
                });
            }
            topology
                .nodes
                .insert(entity_id, GraphEntity::Component(entity));
        }
        for (from, to) in desired.control_connections.iter() {
            topology.links.insert(GraphEntityLink {
                from: GraphEntityId::Component(from.clone()),
                to: GraphEntityId::Component(to.clone()),
                kind: GraphEntityLinkKind::ControlConnection,
            });
        }
        for (from, to) in desired.subscriptions.iter() {
            let id = GraphEntityId::Subscription {
                from: from.clone(),
                to: to.clone(),
            };
            topology.nodes.insert(
                id.clone(),
                GraphEntity::SubscriptionPipe(SubscriptionPipeEntity {
                    from: from.clone(),
                    to: to.clone(),
                    producer_observed: observed.components.get(from).cloned(),
                    consumer_observed: observed.components.get(to).cloned(),
                    producer_requires_downstream_ready: desired.readiness_required.contains(from),
                    consumer_requires_downstream_ready: desired.readiness_required.contains(to),
                }),
            );
            topology.links.insert(GraphEntityLink {
                from: GraphEntityId::Component(from.clone()),
                to: id.clone(),
                kind: GraphEntityLinkKind::SubscriptionInput,
            });
            topology.links.insert(GraphEntityLink {
                from: id.clone(),
                to: GraphEntityId::Component(to.clone()),
                kind: GraphEntityLinkKind::SubscriptionOutput,
            });
            topology.insert_data_dependencies(id, from, to);
        }
        for (id, resource) in &desired.resources {
            topology.nodes.insert(
                GraphEntityId::Resource(id.clone()),
                GraphEntity::Resource(ResourceEntity {
                    desired: resource.clone(),
                    observed: observed.resources.get(id).cloned(),
                }),
            );
        }
        let mut families = BTreeMap::<Arc<str>, PluginFamilyEntity>::new();
        for (identity, dependent_components) in plugins {
            let family =
                families
                    .entry(identity.id.clone())
                    .or_insert_with(|| PluginFamilyEntity {
                        id: identity.id.clone(),
                        versions: BTreeSet::new(),
                        dependent_components: BTreeSet::new(),
                    });
            family.versions.insert(identity.clone());
            family
                .dependent_components
                .extend(dependent_components.iter().cloned());
            topology.links.insert(GraphEntityLink {
                from: GraphEntityId::Plugin(identity.clone()),
                to: GraphEntityId::PluginFamily(identity.id.clone()),
                kind: GraphEntityLinkKind::VersionOfPlugin,
            });
            topology.nodes.insert(
                GraphEntityId::Plugin(identity.clone()),
                GraphEntity::Plugin(PluginEntity {
                    identity,
                    dependent_components,
                }),
            );
        }
        for (id, family) in families {
            topology.nodes.insert(
                GraphEntityId::PluginFamily(id),
                GraphEntity::PluginFamily(family),
            );
        }
        for edge in desired.edges.iter() {
            topology.insert_pipe(PipeEntity {
                relationship: DesiredRelationship {
                    definition: edge.definition.clone(),
                    policy: edge.policy.clone(),
                    pipe: edge.pipe.clone(),
                },
                capabilities: Some(edge.capabilities.clone()),
                resources: edge.resources.clone(),
                declared_only: false,
                observed: observed.relationships.get(&edge.definition).cloned(),
            });
        }
        for relationship in desired.unbound_relationships.iter() {
            if topology
                .nodes
                .contains_key(&GraphEntityId::Pipe(relationship.definition.clone()))
            {
                continue;
            }
            let resources = match &relationship.pipe {
                DesiredPipe::Retained(config) => config.resource_dependencies(),
                DesiredPipe::Qos(config) => config.resource_dependencies(),
                DesiredPipe::Ranked(config) => config.resource_dependencies(),
                DesiredPipe::External { resources, .. } => resources.clone(),
                DesiredPipe::Bounded { .. } | DesiredPipe::Broadcast { .. } => BTreeMap::new(),
            };
            topology.insert_pipe(PipeEntity {
                relationship: relationship.clone(),
                capabilities: None,
                resources,
                declared_only: true,
                observed: observed
                    .relationships
                    .get(&relationship.definition)
                    .cloned(),
            });
        }
        topology
    }

    fn insert_pipe(&mut self, pipe: PipeEntity) {
        let definition = &pipe.relationship.definition;
        let id = GraphEntityId::Pipe(definition.clone());
        self.links.insert(GraphEntityLink {
            from: GraphEntityId::Component(definition.from.component.clone()),
            to: id.clone(),
            kind: GraphEntityLinkKind::PipeInput {
                port: definition.from.port.clone(),
            },
        });
        self.links.insert(GraphEntityLink {
            from: id.clone(),
            to: GraphEntityId::Component(definition.to.component.clone()),
            kind: GraphEntityLinkKind::PipeOutput {
                port: definition.to.port.clone(),
            },
        });
        for resource in pipe.resources.keys() {
            self.links.insert(GraphEntityLink {
                from: id.clone(),
                to: GraphEntityId::Resource(resource.clone()),
                kind: GraphEntityLinkKind::UsesResource,
            });
        }
        self.insert_data_dependencies(
            id.clone(),
            &definition.from.component,
            &definition.to.component,
        );
        self.nodes.insert(id, GraphEntity::Pipe(pipe));
    }

    fn insert_data_dependencies(
        &mut self,
        pipe: GraphEntityId,
        from: &ComponentId,
        to: &ComponentId,
    ) {
        self.links.insert(GraphEntityLink {
            from: GraphEntityId::Component(to.clone()),
            to: pipe.clone(),
            kind: GraphEntityLinkKind::DependsOnData,
        });
        self.links.insert(GraphEntityLink {
            from: pipe,
            to: GraphEntityId::Component(from.clone()),
            kind: GraphEntityLinkKind::DependsOnData,
        });
    }
}
