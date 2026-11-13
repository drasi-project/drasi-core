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
use crate::component_graph::{ComponentKind, ComponentNode, GraphEdge, RelationshipKind};

pub(super) struct ProviderProjection {
    id: GraphEntityId,
    kind: ComponentKind,
    metadata: HashMap<String, String>,
    consumers: Vec<ComponentId>,
    relationship: RelationshipKind,
}

impl ProviderProjection {
    pub(super) fn edges(&self) -> Vec<GraphEdge> {
        self.consumers
            .iter()
            .flat_map(|consumer| {
                [
                    GraphEdge {
                        from: self.id.clone(),
                        to: GraphEntityId::Component(consumer.clone()),
                        relationship: self.relationship.clone(),
                    },
                    GraphEdge {
                        from: GraphEntityId::Component(consumer.clone()),
                        to: self.id.clone(),
                        relationship: self.relationship.reverse(),
                    },
                ]
            })
            .collect()
    }
}

impl Runtime {
    pub(super) fn provider_projections(
        &self,
        records: &BTreeMap<ComponentId, Record>,
    ) -> anyhow::Result<Vec<ProviderProjection>> {
        let mut result = Vec::new();
        for (id, record) in records {
            if let Some(recipe) = &record.bootstrap_recipe {
                let mut metadata = HashMap::from([("kind".into(), recipe.kind.clone())]);
                for (key, value) in &recipe.properties {
                    metadata.insert(key.clone(), serde_json::to_string(value)?);
                }
                result.push(ProviderProjection {
                    id: GraphEntityId::BootstrapProvider(id.clone()),
                    kind: ComponentKind::BootstrapProvider,
                    metadata,
                    consumers: vec![id.clone()],
                    relationship: RelationshipKind::Bootstraps,
                });
            }
        }
        if self.services.identity.is_some() {
            result.push(ProviderProjection {
                id: GraphEntityId::InstanceIdentityProvider,
                kind: ComponentKind::IdentityProvider,
                metadata: HashMap::from([("kind".into(), "identity_provider".into())]),
                consumers: records
                    .iter()
                    .filter(|(_, record)| {
                        matches!(&record.value, Value::Source(_) | Value::Reaction(_))
                    })
                    .map(|(id, _)| id.clone())
                    .collect(),
                relationship: RelationshipKind::Authenticates,
            });
        }
        Ok(result)
    }

    pub(crate) async fn graph_snapshot(
        &self,
        instance_status: ComponentStatus,
    ) -> anyhow::Result<crate::component_graph::GraphSnapshot> {
        let mut snapshot = crate::component_graph::GraphSnapshot {
            instance_id: self.config.id.clone(),
            nodes: vec![ComponentNode {
                id: GraphEntityId::Instance(self.config.id.clone()),
                name: None,
                kind: ComponentKind::Instance,
                status: instance_status,
                metadata: HashMap::new(),
            }],
            edges: Vec::new(),
        };
        let Some(parent) = self.parent.get() else {
            return Ok(snapshot);
        };
        let publication = parent.control().registry_snapshot();
        let records = self.records_at(&publication).await?;
        for (id, record) in &records {
            let observed = publication
                .observed
                .components
                .get(&record.node)
                .ok_or_else(|| anyhow::anyhow!("graph component {id} has no observation"))?;
            let mut metadata = record.metadata.clone();
            let name = publication
                .desired
                .component_by_id(&record.node)
                .ok_or(GraphError::StaleGeneration)?
                .name
                .clone();
            metadata.insert(
                "autoStart".into(),
                publication.desired.lifecycle_policies[&record.node]
                    .auto_start
                    .to_string(),
            );
            let kind = match &record.value {
                Value::Source(source) => {
                    metadata.insert("kind".into(), source.source.type_name().into());
                    ComponentKind::Source
                }
                Value::Reaction(reaction) => {
                    metadata.insert("kind".into(), reaction.reaction.type_name().into());
                    ComponentKind::Reaction
                }
            };
            if let Some(failure) = &observed.failure {
                metadata.insert("error".into(), format!("{:#}", failure.cause));
            }
            snapshot.nodes.push(ComponentNode {
                id: GraphEntityId::Component(id.clone()),
                name: Some(name),
                kind,
                status: inspection::status(record, observed),
                metadata,
            });
            snapshot.edges.extend(ownership_edges(
                &self.config.id,
                &GraphEntityId::Component(id.clone()),
            ));
        }
        for node in publication
            .desired
            .nodes
            .iter()
            .filter(|node| Self::public_component_kind(&publication.desired, node).is_some())
        {
            let id = node.descriptor.id();
            if snapshot
                .nodes
                .iter()
                .any(|node| node.id.component_id() == Some(id))
            {
                continue;
            }
            let kind = Self::public_component_kind(&publication.desired, node)
                .expect("filtered component kind");
            let state = self.component_observation_at(&publication, id, kind)?;
            let mut metadata = HashMap::from([(
                "autoStart".into(),
                publication.desired.lifecycle_policies[id]
                    .auto_start
                    .to_string(),
            )]);
            if kind == "query" {
                metadata.insert(
                    "query".into(),
                    self.native_query_configuration_at(&publication, id)?.query,
                );
            }
            if let Some(failure) = &state.failure {
                metadata.insert("error".into(), format!("{:#}", failure.cause));
            }
            snapshot.nodes.push(ComponentNode {
                id: GraphEntityId::Component(id.clone()),
                name: Some(node.name.clone()),
                kind: match kind {
                    "source" => ComponentKind::Source,
                    "query" => ComponentKind::Query,
                    _ => ComponentKind::Reaction,
                },
                status: events::observed_status(state, ComponentStatus::Added),
                metadata,
            });
            snapshot.edges.extend(ownership_edges(
                &self.config.id,
                &GraphEntityId::Component(id.clone()),
            ));
        }
        let connections: BTreeSet<_> = publication
            .desired
            .data_connections()
            .filter(|(from, to)| {
                snapshot
                    .nodes
                    .iter()
                    .any(|node| node.id.component_id() == Some(from))
                    && snapshot
                        .nodes
                        .iter()
                        .any(|node| node.id.component_id() == Some(to))
            })
            .collect();
        for (from, to) in connections {
            snapshot.edges.extend([
                GraphEdge {
                    from: GraphEntityId::Component(from.clone()),
                    to: GraphEntityId::Component(to.clone()),
                    relationship: RelationshipKind::Feeds,
                },
                GraphEdge {
                    from: GraphEntityId::Component(to.clone()),
                    to: GraphEntityId::Component(from.clone()),
                    relationship: RelationshipKind::SubscribesTo,
                },
            ]);
        }
        for provider in self.provider_projections(&records)? {
            snapshot.edges.extend(provider.edges());
            snapshot
                .edges
                .extend(ownership_edges(&self.config.id, &provider.id));
            snapshot.nodes.push(ComponentNode {
                id: provider.id,
                name: None,
                kind: provider.kind,
                status: ComponentStatus::Running,
                metadata: provider.metadata,
            });
        }
        Ok(snapshot)
    }
}

pub(super) fn ownership_edges(instance: &str, id: &GraphEntityId) -> [GraphEdge; 2] {
    [
        GraphEdge {
            from: GraphEntityId::Instance(instance.to_owned()),
            to: id.clone(),
            relationship: RelationshipKind::Owns,
        },
        GraphEdge {
            from: id.clone(),
            to: GraphEntityId::Instance(instance.to_owned()),
            relationship: RelationshipKind::OwnedBy,
        },
    ]
}
