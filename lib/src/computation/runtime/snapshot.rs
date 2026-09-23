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
    id: String,
    kind: ComponentKind,
    metadata: HashMap<String, String>,
    consumers: Vec<String>,
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
                        to: consumer.clone(),
                        relationship: self.relationship.clone(),
                    },
                    GraphEdge {
                        from: consumer.clone(),
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
        records: &BTreeMap<String, Record>,
    ) -> anyhow::Result<Vec<ProviderProjection>> {
        let mut result = Vec::new();
        for (id, record) in records {
            if let Some(recipe) = &record.bootstrap_recipe {
                let mut metadata = HashMap::from([("kind".into(), recipe.kind.clone())]);
                for (key, value) in &recipe.properties {
                    metadata.insert(key.clone(), serde_json::to_string(value)?);
                }
                result.push(ProviderProjection {
                    id: format!("{id}-bootstrap"),
                    kind: ComponentKind::BootstrapProvider,
                    metadata,
                    consumers: vec![id.clone()],
                    relationship: RelationshipKind::Bootstraps,
                });
            }
        }
        if self.services.identity.is_some() {
            result.push(ProviderProjection {
                id: "identity-provider".into(),
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
    ) -> anyhow::Result<crate::component_graph::GraphSnapshot> {
        let mut snapshot = crate::component_graph::GraphSnapshot {
            instance_id: self.config.id.clone(),
            nodes: vec![ComponentNode {
                id: self.config.id.clone(),
                kind: ComponentKind::Instance,
                status: ComponentStatus::Running,
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
                Value::Query(query) => {
                    metadata.insert("query".into(), query.config.query.clone());
                    ComponentKind::Query
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
                id: id.clone(),
                kind,
                status: inspection::status(record, observed),
                metadata,
            });
            snapshot.edges.extend(ownership_edges(&self.config.id, id));
        }
        for (from, to) in publication.desired.subscriptions.iter() {
            snapshot.edges.extend([
                GraphEdge {
                    from: from.to_string(),
                    to: to.to_string(),
                    relationship: RelationshipKind::Feeds,
                },
                GraphEdge {
                    from: to.to_string(),
                    to: from.to_string(),
                    relationship: RelationshipKind::SubscribesTo,
                },
            ]);
        }
        for provider in self.provider_projections(&records)? {
            // Provider recipes are inspection metadata, never runtime members.
            // Do not mask a graph-owned component with the same public ID.
            if snapshot.nodes.iter().any(|node| node.id == provider.id) {
                continue;
            }
            snapshot.edges.extend(provider.edges());
            snapshot
                .edges
                .extend(ownership_edges(&self.config.id, &provider.id));
            snapshot.nodes.push(ComponentNode {
                id: provider.id,
                kind: provider.kind,
                status: ComponentStatus::Running,
                metadata: provider.metadata,
            });
        }
        Ok(snapshot)
    }
}

fn ownership_edges(instance: &str, id: &str) -> [GraphEdge; 2] {
    [
        GraphEdge {
            from: instance.into(),
            to: id.into(),
            relationship: RelationshipKind::Owns,
        },
        GraphEdge {
            from: id.into(),
            to: instance.into(),
            relationship: RelationshipKind::OwnedBy,
        },
    ]
}
