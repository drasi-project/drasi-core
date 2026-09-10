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

use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Arc,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DesiredPipe {
    Bounded {
        capacity: usize,
    },
    Broadcast {
        capacity: usize,
        lag_policy: crate::computation::v1::BroadcastLagPolicy,
    },
    Retained(crate::computation::v1::RetainedPipeConfig),
    External {
        binding: String,
        capabilities: Vec<PipeCapability>,
        capacity: Option<usize>,
        #[serde(default)]
        resources: BTreeMap<ResourceId, ResourceRole>,
        #[serde(default)]
        exclusive_resources: Vec<ResourceId>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ComponentConstruction {
    Factory(ComponentSpecification),
    External { binding: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredComponent {
    pub descriptor: ComponentDescriptor,
    pub role: ComponentRole,
    pub completion: Option<SinkCompletion>,
    pub streams: BTreeMap<PortId, StreamId>,
    pub lifecycle: LifecyclePolicy,
    pub input_merge: InputMergePolicy,
    pub construction: ComponentConstruction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredRelationship {
    pub definition: EdgeDefinition,
    pub policy: RelationshipPolicy,
    pub pipe: DesiredPipe,
}

/// Versioned desired configuration only. Operational state, generations, secrets,
/// queued data and constructed handles have no fields in this representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredTopology {
    pub version: u32,
    pub graph_id: String,
    pub revision: GraphRevision,
    pub components: Vec<DesiredComponent>,
    pub relationships: Vec<DesiredRelationship>,
    pub resources: Vec<ResourceSpecification>,
    pub requirements: PipeRequirements,
    /// Desired links crossing an exact/subset selection boundary are explicit.
    pub boundary_relationships: Vec<DesiredRelationship>,
}

#[derive(Default, Clone)]
pub struct FactoryRegistry {
    pub(super) factories: BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
}

impl FactoryRegistry {
    pub fn register(&mut self, factory: Arc<dyn ComponentFactory>) -> GraphResult<()> {
        let id = factory.descriptor().implementation.clone();
        if self.factories.contains_key(&id) {
            return Err(topology("duplicate component implementation"));
        }
        self.factories.insert(id, factory);
        Ok(())
    }
    pub fn standard() -> Self {
        let factories: Vec<Arc<dyn ComponentFactory>> = vec![
            Arc::new(crate::computation::v1::ContinuousQueryFactory::default()),
            Arc::new(crate::computation::v1::LegacySourceFactory::default()),
            Arc::new(crate::computation::v1::SourcePluginAdapterFactory::default()),
            Arc::new(crate::computation::v1::LegacyReactionFactory::default()),
            Arc::new(crate::computation::v1::ReactionPluginAdapterFactory::default()),
            Arc::new(crate::computation::v1::QueryReplayFactory::default()),
            Arc::new(crate::computation::v1::QueryResultsOutletFactory::default()),
            Arc::new(crate::computation::v1::WalReplaySourceFactory::default()),
            Arc::new(crate::computation::v1::ComputationTopologyFactory::default()),
        ];
        Self {
            factories: factories
                .into_iter()
                .map(|factory| (factory.descriptor().implementation.clone(), factory))
                .collect(),
        }
    }
}

#[derive(Default)]
pub struct TopologyBindings {
    pub factories: FactoryRegistry,
    pub components: BTreeMap<String, ConstructedComponent>,
    pub pipes: BTreeMap<String, Box<dyn PipeProvider>>,
    pub resources: BTreeMap<ResourceId, ResourceHandle>,
}

impl DesiredPipe {
    pub(super) fn resource_dependencies(&self) -> BTreeMap<ResourceId, ResourceRole> {
        match self {
            Self::Retained(config) => config.resource_dependencies(),
            Self::External { resources, .. } => resources.clone(),
            Self::Bounded { .. } | Self::Broadcast { .. } => BTreeMap::new(),
        }
    }

    pub(super) fn resolve(
        &self,
        bindings: &mut TopologyBindings,
    ) -> GraphResult<Box<dyn PipeProvider>> {
        Ok(match self {
            Self::Bounded { capacity } => Box::new(crate::computation::v1::BoundedPipeConfig {
                capacity: *capacity,
            }),
            Self::Broadcast {
                capacity,
                lag_policy,
            } => Box::new(crate::computation::v1::BroadcastPipeConfig {
                capacity: *capacity,
                lag_policy: *lag_policy,
            }),
            Self::Retained(config) => Box::new(config.clone()),
            Self::External {
                binding,
                capabilities,
                capacity,
                resources,
                exclusive_resources,
            } => {
                let provider = bindings.pipes.remove(binding).ok_or_else(|| {
                    topology(format!("external pipe binding {binding} was not supplied"))
                })?;
                let capacity = capacity
                    .map(|capacity| {
                        NonZeroUsize::new(capacity)
                            .ok_or_else(|| topology("invalid external pipe capacity"))
                    })
                    .transpose()?;
                let expected = PipeCapabilities::try_new(capabilities.iter().copied(), capacity)?;
                if provider
                    .capabilities()
                    .map_err(|source| GraphError::Pipe { edge: 0, source })?
                    != expected
                {
                    return Err(topology(format!(
                        "external pipe binding {binding} changed its capabilities"
                    )));
                }
                if provider.resource_dependencies() != *resources
                    || provider
                        .exclusive_resources()
                        .into_iter()
                        .collect::<BTreeSet<_>>()
                        != exclusive_resources.iter().cloned().collect::<BTreeSet<_>>()
                {
                    return Err(topology(format!(
                        "external pipe binding {binding} changed its resource dependencies"
                    )));
                }
                provider
            }
        })
    }
}

impl DesiredTopology {
    pub fn to_json(&self) -> GraphResult<String> {
        serde_json::to_string_pretty(self)
            .map_err(|error| topology(format!("cannot export desired topology: {error}")))
    }
    pub fn from_json(value: &str) -> GraphResult<Self> {
        let topology: Self = serde_json::from_str(value)
            .map_err(|error| super::topology(format!("invalid desired topology: {error}")))?;
        if topology.version != 1 {
            return Err(super::topology("unsupported desired topology version"));
        }
        validate_identifier("graph", &topology.graph_id)?;
        Ok(topology)
    }
    pub fn build(&self, mut bindings: TopologyBindings) -> GraphResult<ComputationGraph> {
        if self.version != 1 {
            return Err(topology("unsupported desired topology version"));
        }
        let mut builder = ComputationGraph::builder(self.graph_id.as_str())
            .requirements(self.requirements.clone());
        builder.unbound_relationships = self.boundary_relationships.clone();
        for resource in &self.resources {
            builder = builder.declare_resource(resource.clone())?;
            if let Some(handle) = bindings.resources.remove(&resource.id) {
                builder = builder.provide_resource(resource.id.clone(), handle)?;
            }
        }
        for component in &self.components {
            match &component.construction {
                ComponentConstruction::Factory(spec) => {
                    if spec.descriptor != component.descriptor
                        || spec.role != component.role
                        || spec.completion != component.completion
                    {
                        return Err(topology(
                            "component specification and desired interface disagree",
                        ));
                    }
                    let factory = bindings
                        .factories
                        .factories
                        .get(&spec.implementation)
                        .cloned()
                        .ok_or_else(|| {
                            topology(format!(
                                "component implementation {} is not registered",
                                spec.implementation.name
                            ))
                        })?;
                    builder = builder.component(spec.clone(), factory);
                }
                ComponentConstruction::External { binding } => {
                    let instance = bindings.components.remove(binding).ok_or_else(|| {
                        topology(format!(
                            "external component binding {binding} was not supplied"
                        ))
                    })?;
                    if instance.0.descriptor() != &component.descriptor
                        || instance.0.role() != component.role
                        || instance.0.completion() != component.completion
                    {
                        return Err(topology(format!(
                            "external component {binding} does not match its desired interface"
                        )));
                    }
                    builder.components.push(instance.0);
                }
            }
            builder = builder
                .lifecycle_policy(
                    component.descriptor.id().clone(),
                    component.lifecycle.clone(),
                )
                .input_merge(component.descriptor.id().clone(), component.input_merge);
            for (port, stream) in &component.streams {
                builder = builder.bind_stream(
                    Endpoint::new(component.descriptor.id().clone(), port.clone()),
                    stream.clone(),
                );
            }
        }
        for relationship in &self.relationships {
            let pipe = relationship.pipe.resolve(&mut bindings)?;
            builder = builder
                .connect(relationship.definition.clone(), pipe)
                .relationship_policy(relationship.definition.clone(), relationship.policy.clone());
        }
        let mut graph = builder.build()?;
        graph.snapshot.external_bindings = self
            .components
            .iter()
            .filter_map(|component| {
                if let ComponentConstruction::External { binding } = &component.construction {
                    Some((
                        component.descriptor.id().clone(),
                        Arc::from(binding.as_str()),
                    ))
                } else {
                    None
                }
            })
            .collect();
        graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
        Ok(graph)
    }
}

pub(super) fn selected(
    snapshot: &GraphSnapshot,
    selection: &crate::computation::v1::GraphSelection,
) -> GraphResult<BTreeSet<ComponentId>> {
    use crate::computation::v1::GraphSelection;
    let all: BTreeSet<_> = snapshot
        .nodes
        .iter()
        .map(|node| node.descriptor.id().clone())
        .collect();
    let (ids, direction) = match selection {
        GraphSelection::All => return Ok(all),
        GraphSelection::Exact(ids) => (ids, 0),
        GraphSelection::Dependencies(ids) => (ids, -1),
        GraphSelection::Dependents(ids) => (ids, 1),
    };
    let mut selected = BTreeSet::new();
    for id in ids {
        if !all.contains(id) {
            return Err(topology(format!("unknown component {id}")));
        }
        selected.insert(id.clone());
    }
    loop {
        if direction == 0 {
            break;
        }
        let before = selected.len();
        for edge in snapshot.edges.iter() {
            let (from, to) = if direction < 0 {
                (
                    &edge.definition.to.component,
                    &edge.definition.from.component,
                )
            } else {
                (
                    &edge.definition.from.component,
                    &edge.definition.to.component,
                )
            };
            if selected.contains(from) {
                selected.insert(to.clone());
            }
        }
        if selected.len() == before {
            break;
        }
    }
    Ok(selected)
}

impl GraphSnapshot {
    pub fn select(
        &self,
        selection: crate::computation::v1::GraphSelection,
    ) -> GraphResult<DesiredTopology> {
        let selected = selected(self, &selection)?;
        let mut resources = BTreeSet::new();
        let components = self
            .nodes
            .iter()
            .filter(|node| selected.contains(node.descriptor.id()))
            .map(|node| {
                let construction = match self.specifications.get(node.descriptor.id()) {
                    Some(spec) => {
                        resources.extend(spec.dependencies.values().flatten().cloned());
                        resources.extend(spec.configuration.values().filter_map(|value| {
                            if let ConfigurationValue::Reference { resource, .. } = value {
                                Some(resource.clone())
                            } else {
                                None
                            }
                        }));
                        ComponentConstruction::Factory(spec.clone())
                    }
                    None => ComponentConstruction::External {
                        binding: self.external_bindings[node.descriptor.id()].to_string(),
                    },
                };
                DesiredComponent {
                    descriptor: node.descriptor.clone(),
                    role: node.role,
                    completion: node.completion,
                    streams: node.output_streams.clone(),
                    lifecycle: self.lifecycle_policies[node.descriptor.id()].clone(),
                    input_merge: node.input_merge,
                    construction,
                }
            })
            .collect();
        let mut relationships = Vec::new();
        let mut boundary_relationships: Vec<_> = self
            .unbound_relationships
            .iter()
            .filter(|edge| {
                selected.contains(&edge.definition.from.component)
                    || selected.contains(&edge.definition.to.component)
            })
            .cloned()
            .collect();
        for edge in &boundary_relationships {
            resources.extend(edge.pipe.resource_dependencies().into_keys());
        }
        for edge in self.edges.iter() {
            let from = selected.contains(&edge.definition.from.component);
            let to = selected.contains(&edge.definition.to.component);
            if from || to {
                resources.extend(edge.resources.keys().cloned());
                let relationship = DesiredRelationship {
                    definition: edge.definition.clone(),
                    policy: edge.policy.clone(),
                    pipe: edge.pipe.clone(),
                };
                if from && to {
                    relationships.push(relationship);
                } else {
                    boundary_relationships.push(relationship);
                }
            }
        }
        if matches!(selection, crate::computation::v1::GraphSelection::All) {
            resources.extend(self.resources.keys().cloned());
        }
        Ok(DesiredTopology {
            version: 1,
            graph_id: self.id.to_string(),
            revision: self.revision,
            components,
            relationships,
            resources: self
                .resources
                .iter()
                .filter(|(id, _)| resources.contains(*id))
                .map(|(_, spec)| spec.clone())
                .collect(),
            requirements: self.requirements.clone(),
            boundary_relationships,
        })
    }
}
