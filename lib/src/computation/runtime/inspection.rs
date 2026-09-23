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
use crate::{
    channels::{ComponentEvent, ComponentType},
    config::{QueryRuntime, ReactionRuntime, SourceRuntime},
    managers::{ComponentLogKey, LogMessage},
};

pub(super) fn status(record: &Record, observed: &ObservedComponent) -> ComponentStatus {
    events::observed_status(observed, record.initial_status)
}

fn error_message(record: &Record, observed: &ObservedComponent) -> Option<String> {
    if status(record, observed) != ComponentStatus::Error {
        return None;
    }
    observed
        .failure
        .as_ref()
        .map(|failure| format!("{:#}", failure.cause))
        .or_else(|| match &record.value {
            Value::Query(query) => query.last_error(),
            _ => None,
        })
}

impl Runtime {
    pub(crate) async fn component_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.events.component(id)
    }

    pub(crate) async fn events(&self, kind: Option<ComponentType>) -> Vec<ComponentEvent> {
        self.events
            .all()
            .into_iter()
            .filter(|event| {
                matches!(
                    event.component_type,
                    ComponentType::Source | ComponentType::Query | ComponentType::Reaction
                ) && kind
                    .as_ref()
                    .map_or(true, |kind| kind == &event.component_type)
            })
            .collect()
    }

    pub(crate) async fn inventory(&self) -> anyhow::Result<ComputationInventory> {
        let publication = self.control()?.registry_snapshot();
        let records = self.records_at(&publication).await?;
        let root = ComputationScope::root(publication.desired.id.clone());
        let mut inventory = ComputationInventory::new(self.config.id.as_str());
        inventory.insert(
            root.clone(),
            None,
            ComputationTopology::new(&publication.desired, &publication.observed),
        )?;
        for record in records.values() {
            let Value::Query(query) = &record.value else {
                continue;
            };
            let Some((execution, sources)) = query.execution_inspection() else {
                continue;
            };
            let scope = root.nested(record.node.clone());
            inventory.insert(
                scope.clone(),
                Some(ComputationScopeOwner {
                    component: root.entity(GraphEntityId::Component(record.node.clone())),
                    generation: publication.observed.components[&record.node].generation,
                }),
                execution.topology(),
            )?;
            for source in sources {
                let Some(current) = records.get(source.source.id()) else {
                    continue;
                };
                if !matches!(&current.value, Value::Source(current) if Arc::ptr_eq(current, source))
                {
                    continue;
                }
                inventory.links.insert(ScopedGraphEntityLink {
                    from: scope.entity(GraphEntityId::Resource(
                        ComputationPipelineBuilder::source_resource_id(source.source.id())?,
                    )),
                    to: root.entity(GraphEntityId::Component(current.node.clone())),
                    kind: GraphEntityLinkKind::UsesComponent,
                });
            }
            for resource in self.services.dependencies().values().flatten() {
                if execution.desired.resources.contains_key(resource)
                    && publication.desired.resources.contains_key(resource)
                {
                    inventory.links.insert(ScopedGraphEntityLink {
                        from: scope.entity(GraphEntityId::Resource(resource.clone())),
                        to: root.entity(GraphEntityId::Resource(resource.clone())),
                        kind: GraphEntityLinkKind::UsesResource,
                    });
                }
            }
            if let Some(providers) = publication.desired.specifications[&record.node]
                .dependencies
                .get("indexes")
            {
                for provider in providers {
                    inventory.links.insert(ScopedGraphEntityLink {
                        from: scope.entity(GraphEntityId::Resource(
                            ComputationPipelineBuilder::index_resource_id(&query.config.id)?,
                        )),
                        to: root.entity(GraphEntityId::Resource(provider.clone())),
                        kind: GraphEntityLinkKind::UsesResource,
                    });
                }
            }
        }
        Ok(inventory)
    }

    async fn inspect_record(
        &self,
        id: &str,
        kind: &'static str,
    ) -> anyhow::Result<(Record, ObservedComponent)> {
        let publication = self.control()?.registry_snapshot();
        let record = self
            .records_at(&publication)
            .await?
            .remove(id)
            .filter(|record| record.value.instance().kind() == kind)
            .ok_or_else(|| crate::managers::ComponentNotFoundError::new(kind, id))?;
        let observed = publication
            .observed
            .components
            .get(&record.node)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("graph component {id} has no observation"))?;
        Ok((record, observed))
    }

    pub(crate) async fn list_components(
        &self,
        kind: &str,
    ) -> anyhow::Result<Vec<(String, ComponentStatus)>> {
        let publication = self.control()?.registry_snapshot();
        self.records_at(&publication)
            .await?
            .into_iter()
            .filter(|(_, record)| record.value.instance().kind() == kind)
            .map(|(id, record)| {
                let observed = publication
                    .observed
                    .components
                    .get(&record.node)
                    .ok_or_else(|| anyhow::anyhow!("graph component {id} has no observation"))?;
                Ok((id, status(&record, observed)))
            })
            .collect()
    }

    pub(crate) async fn component_status(
        &self,
        id: &str,
        kind: &'static str,
    ) -> anyhow::Result<ComponentStatus> {
        let (record, observed) = self.inspect_record(id, kind).await?;
        Ok(status(&record, &observed))
    }

    pub(crate) async fn source_info(&self, id: &str) -> anyhow::Result<SourceRuntime> {
        let (record, observed) = self.inspect_record(id, "source").await?;
        let Value::Source(source) = &record.value else {
            unreachable!("source record");
        };
        Ok(SourceRuntime {
            id: id.to_owned(),
            source_type: source.source.type_name().to_owned(),
            status: status(&record, &observed),
            error_message: error_message(&record, &observed),
            properties: source.source.properties(),
        })
    }

    pub(crate) async fn query_info(&self, id: &str) -> anyhow::Result<QueryRuntime> {
        let (record, observed) = self.inspect_record(id, "query").await?;
        let Value::Query(query) = &record.value else {
            unreachable!("query record");
        };
        Ok(QueryRuntime {
            id: id.to_owned(),
            query: query.config.query.clone(),
            status: status(&record, &observed),
            error_message: error_message(&record, &observed),
            source_subscriptions: query.config.sources.clone(),
            joins: query.config.joins.clone(),
        })
    }

    pub(crate) async fn reaction_info(&self, id: &str) -> anyhow::Result<ReactionRuntime> {
        let (record, observed) = self.inspect_record(id, "reaction").await?;
        let Value::Reaction(reaction) = &record.value else {
            unreachable!("reaction record");
        };
        Ok(ReactionRuntime {
            id: id.to_owned(),
            reaction_type: reaction.reaction.type_name().to_owned(),
            status: status(&record, &observed),
            error_message: error_message(&record, &observed),
            queries: reaction.query_ids.clone(),
            properties: reaction.reaction.properties(),
        })
    }

    pub(crate) async fn query_configurations(&self) -> anyhow::Result<Vec<QueryConfig>> {
        let publication = self.control()?.registry_snapshot();
        Ok(self
            .records_at(&publication)
            .await?
            .into_values()
            .filter_map(|record| match record.value {
                Value::Query(query) => {
                    let mut config = query.config.clone();
                    config.auto_start =
                        publication.desired.lifecycle_policies[&record.node].auto_start;
                    Some(config)
                }
                _ => None,
            })
            .collect())
    }

    pub(crate) async fn query_configuration(&self, id: &str) -> anyhow::Result<QueryConfig> {
        self.query_configurations()
            .await?
            .into_iter()
            .find(|config| config.id == id)
            .ok_or_else(|| crate::managers::ComponentNotFoundError::new("query", id).into())
    }

    pub(crate) async fn subscribe_logs(
        &self,
        id: &str,
        kind: &str,
    ) -> anyhow::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        self.record(id, kind).await?;
        let component_type = match kind {
            "source" => ComponentType::Source,
            "query" => ComponentType::Query,
            "reaction" => ComponentType::Reaction,
            _ => anyhow::bail!("unsupported component log kind {kind}"),
        };
        Ok(self
            .logs
            .subscribe_by_key(&ComponentLogKey::new(&self.config.id, component_type, id))
            .await)
    }

    pub(crate) async fn subscribe_events(
        &self,
        id: &str,
        kind: &str,
    ) -> anyhow::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.record(id, kind).await?;
        Ok(self.events.subscribe(id))
    }

    pub(crate) async fn configuration_snapshot(
        &self,
    ) -> anyhow::Result<crate::config::snapshot::ConfigurationSnapshot> {
        use crate::{
            component_graph::{GraphEdge, RelationshipKind},
            config::snapshot::{
                ConfigurationSnapshot, QuerySnapshot, ReactionSnapshot, SourceSnapshot,
            },
        };
        let publication = self.control()?.registry_snapshot();
        let records = self.records_at(&publication).await?;
        let mut snapshot = ConfigurationSnapshot {
            instance_id: self.config.id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            sources: Vec::new(),
            queries: Vec::new(),
            reactions: Vec::new(),
            edges: Vec::new(),
        };
        for (id, record) in &records {
            let observed = publication
                .observed
                .components
                .get(&record.node)
                .ok_or_else(|| anyhow::anyhow!("graph component {id} has no observation"))?;
            let status = status(record, observed);
            let auto_start = publication.desired.lifecycle_policies[&record.node].auto_start;
            match &record.value {
                Value::Source(source) => {
                    snapshot.sources.push(SourceSnapshot {
                        id: id.clone(),
                        source_type: source.source.type_name().to_owned(),
                        status,
                        auto_start,
                        properties: source.source.properties(),
                        bootstrap_provider: record.bootstrap_recipe.clone(),
                    });
                }
                Value::Query(query) => {
                    let mut config = query.config.clone();
                    config.auto_start = auto_start;
                    snapshot.queries.push(QuerySnapshot {
                        id: id.clone(),
                        config,
                        status,
                    });
                }
                Value::Reaction(reaction) => snapshot.reactions.push(ReactionSnapshot {
                    id: id.clone(),
                    reaction_type: reaction.reaction.type_name().to_owned(),
                    status,
                    auto_start,
                    queries: reaction.query_ids.clone(),
                    properties: reaction.reaction.properties(),
                }),
            }
            snapshot.edges.extend([
                GraphEdge {
                    from: self.config.id.clone(),
                    to: id.clone(),
                    relationship: RelationshipKind::Owns,
                },
                GraphEdge {
                    from: id.clone(),
                    to: self.config.id.clone(),
                    relationship: RelationshipKind::OwnedBy,
                },
            ]);
        }
        for (from, to) in publication.desired.subscriptions.iter() {
            snapshot.edges.push(GraphEdge {
                from: from.to_string(),
                to: to.to_string(),
                relationship: RelationshipKind::Feeds,
            });
            if matches!(
                (
                    records.get(from.as_str()).map(|record| &record.value),
                    records.get(to.as_str()).map(|record| &record.value)
                ),
                (Some(Value::Source(_)), Some(Value::Query(_)))
                    | (Some(Value::Query(_)), Some(Value::Reaction(_)))
            ) {
                snapshot.edges.push(GraphEdge {
                    from: to.to_string(),
                    to: from.to_string(),
                    relationship: RelationshipKind::SubscribesTo,
                });
            }
        }
        for provider in self.provider_projections(&records)? {
            snapshot.edges.extend(provider.edges());
        }
        Ok(snapshot)
    }
}
