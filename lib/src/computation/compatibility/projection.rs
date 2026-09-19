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
use crate::component_graph::{ComponentKind, GraphEdge, RelationshipKind};

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

    pub(super) fn project_provider_metadata(
        &self,
        projection: &mut ComponentGraph,
        records: &BTreeMap<String, Record>,
    ) -> anyhow::Result<()> {
        for provider in self.provider_projections(records)? {
            if let Some(node) = projection.get_component_mut(&provider.id) {
                anyhow::ensure!(
                    node.kind == provider.kind,
                    "legacy provider view '{}' conflicts with a component; native bindings are unchanged",
                    provider.id,
                );
                node.metadata = provider.metadata;
                for consumer in provider.consumers {
                    projection.add_relationship(
                        &provider.id,
                        &consumer,
                        provider.relationship.clone(),
                    )?;
                }
            } else if provider.kind == ComponentKind::BootstrapProvider {
                projection.register_bootstrap_provider(
                    &provider.id,
                    provider.metadata,
                    &provider.consumers,
                )?;
            } else {
                projection.register_identity_provider(
                    &provider.id,
                    provider.metadata,
                    &provider.consumers,
                )?;
            }
        }
        Ok(())
    }
}
