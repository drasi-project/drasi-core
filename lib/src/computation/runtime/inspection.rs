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
    pub(crate) async fn query_inspector(&self, id: &str) -> anyhow::Result<ComputationInspector> {
        let publication = self.control()?.registry_snapshot();
        self.query_observation_at(&publication, id)?;
        match self
            .records_at(&publication)
            .await?
            .get(id)
            .map(|record| &record.value)
        {
            Some(Value::Query(query)) => Ok(query.inspector()),
            _ => self.inspector(),
        }
    }

    pub(super) fn is_query_node(node: &NodeSnapshot) -> bool {
        node.descriptor
            .semantic_kind()
            .unwrap_or_else(|| node.role.into())
            == ComponentSemanticKind::Query
    }

    pub(super) fn public_component_kind(
        snapshot: &GraphSnapshot,
        node: &NodeSnapshot,
    ) -> Option<&'static str> {
        let kind = node
            .descriptor
            .semantic_kind()
            .unwrap_or_else(|| node.role.into());
        if node.descriptor.semantic_kind().is_none() {
            if let Some(spec) = snapshot.specifications.get(node.descriptor.id()) {
                match spec.implementation.name.as_ref() {
                    "drasi/source-plugin-subscription" | "drasi/query-scheduled-source" => {
                        return None
                    }
                    "drasi/reaction-plugin" => return Some("reaction"),
                    _ => {}
                }
            }
        }
        match kind {
            ComponentSemanticKind::Source => Some("source"),
            ComponentSemanticKind::Query => Some("query"),
            ComponentSemanticKind::Reaction => Some("reaction"),
            _ => None,
        }
    }

    pub(super) fn component_observation_at<'a>(
        &self,
        publication: &'a GraphRegistrySnapshot,
        id: &str,
        kind: &'static str,
    ) -> anyhow::Result<&'a ObservedComponent> {
        let node = publication
            .desired
            .nodes
            .iter()
            .find(|node| {
                node.descriptor.id().as_str() == id
                    && Self::public_component_kind(&publication.desired, node) == Some(kind)
            })
            .ok_or_else(|| crate::managers::ComponentNotFoundError::new(kind, id))?;
        publication
            .observed
            .components
            .get(node.descriptor.id())
            .ok_or_else(|| anyhow::anyhow!("{kind} '{id}' has no graph observation"))
    }

    fn native_component_properties(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &str,
        kind: &'static str,
    ) -> anyhow::Result<(String, HashMap<String, serde_json::Value>)> {
        self.component_observation_at(publication, id, kind)?;
        let component = ComponentId::try_new(id)?;
        let configuration = publication
            .component_configuration(&component)
            .ok_or_else(|| anyhow::anyhow!("{kind} '{id}' has no published configuration"))?;
        let declared = |values: &BTreeMap<Arc<str>, ConfigurationValue>| {
            values.iter().map(|(name, value)| match value {
                ConfigurationValue::Literal(value) => Ok((name.to_string(), value.clone())),
                ConfigurationValue::Reference { .. } => anyhow::bail!("{kind} '{id}' configuration references have not been resolved; use the computation configuration export"),
            }).collect::<anyhow::Result<HashMap<_, _>>>()
        };
        let properties = match configuration {
            CapturedComponentConfiguration::Available { values } => {
                serde_json::from_value(values.clone())?
            }
            CapturedComponentConfiguration::Declared { values } => declared(values)?,
            CapturedComponentConfiguration::Unavailable { reason } => {
                let specification = publication
                    .desired
                    .specifications
                    .get(&component)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "{kind} '{id}' exists, but its configuration is unavailable: {reason}"
                        )
                    })?;
                declared(&specification.configuration)?
            }
        };
        let implementation = publication
            .desired
            .specifications
            .get(&component)
            .map(|spec| spec.implementation.name.to_string())
            .or_else(|| {
                publication
                    .desired
                    .nodes
                    .iter()
                    .find(|node| node.descriptor.id() == &component)
                    .and_then(|node| node.descriptor.plugin_identity())
                    .map(|plugin| plugin.id.to_string())
            })
            .unwrap_or_else(|| "native".into());
        Ok((implementation, properties))
    }

    pub(super) fn native_query_configuration_at(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &str,
    ) -> anyhow::Result<QueryConfig> {
        let node = publication
            .desired
            .nodes
            .iter()
            .find(|node| node.descriptor.id().as_str() == id && Self::is_query_node(node))
            .ok_or_else(|| crate::managers::ComponentNotFoundError::new("query", id))?;
        let component = node.descriptor.id();
        let mut config = if let Some(api) = publication.query_api(component) {
            api.config.clone()
        } else if let Some(ConfigurationValue::Literal(value)) = publication
            .desired
            .specifications
            .get(component)
            .and_then(|spec| spec.configuration.get("query_config"))
        {
            serde_json::from_value(value.clone())?
        } else if let Some(spec) =
            publication
                .desired
                .specifications
                .get(component)
                .filter(|spec| {
                    spec.implementation
                        == ContinuousQueryFactory::default()
                            .descriptor()
                            .implementation
                })
        {
            ContinuousQueryFactory::query_configuration(spec, &publication.desired.id)?
        } else {
            anyhow::bail!("Query '{id}' exists but does not expose ordinary query configuration in its current construction state");
        };
        anyhow::ensure!(
            config.id == id,
            "query configuration identity does not match its graph node"
        );
        config.auto_start = publication.desired.lifecycle_policies[component].auto_start;
        Ok(config)
    }

    pub(super) fn query_observation_at<'a>(
        &self,
        publication: &'a GraphRegistrySnapshot,
        id: &str,
    ) -> anyhow::Result<&'a ObservedComponent> {
        self.component_observation_at(publication, id, "query")
    }

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
        let records = self.records_at(&publication).await?;
        let mut components: Vec<_> = publication
            .desired
            .nodes
            .iter()
            .filter(|node| Self::public_component_kind(&publication.desired, node) == Some(kind))
            .map(|node| {
                let id = node.descriptor.id().as_str();
                let state = publication
                    .observed
                    .components
                    .get(node.descriptor.id())
                    .ok_or_else(|| anyhow::anyhow!("{kind} '{id}' has no graph observation"))?;
                Ok((
                    id.to_owned(),
                    records.get(id).filter(|_| kind != "query").map_or_else(
                        || events::observed_status(state, ComponentStatus::Added),
                        |record| status(record, state),
                    ),
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        components.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(components)
    }

    pub(crate) async fn component_status(
        &self,
        id: &str,
        kind: &'static str,
    ) -> anyhow::Result<ComponentStatus> {
        let publication = self.control()?.registry_snapshot();
        if kind != "query" {
            if let Some(record) = self
                .records_at(&publication)
                .await?
                .get(id)
                .filter(|record| record.value.instance().kind() == kind)
            {
                return Ok(status(
                    record,
                    self.component_observation_at(&publication, id, kind)?,
                ));
            }
        }
        Ok(events::observed_status(
            self.component_observation_at(&publication, id, kind)?,
            ComponentStatus::Added,
        ))
    }

    pub(crate) async fn source_info(&self, id: &str) -> anyhow::Result<SourceRuntime> {
        let publication = self.control()?.registry_snapshot();
        if !self.records_at(&publication).await?.contains_key(id) {
            return self.native_source_info_at(&publication, id);
        }
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
        let publication = self.control()?.registry_snapshot();
        let records = self.records_at(&publication).await?;
        let config = match records.get(id).map(|record| &record.value) {
            Some(Value::Query(query)) => query.config.clone(),
            _ => self.native_query_configuration_at(&publication, id)?,
        };
        let observed = self.query_observation_at(&publication, id)?;
        Ok(QueryRuntime {
            id: id.to_owned(),
            query: config.query,
            status: events::observed_status(observed, ComponentStatus::Added),
            error_message: observed
                .failure
                .as_ref()
                .map(|failure| format!("{:#}", failure.cause)),
            source_subscriptions: config.sources,
            joins: config.joins,
        })
    }

    pub(crate) async fn reaction_info(&self, id: &str) -> anyhow::Result<ReactionRuntime> {
        let publication = self.control()?.registry_snapshot();
        if !self.records_at(&publication).await?.contains_key(id) {
            return self.native_reaction_info_at(&publication, id);
        }
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

    fn native_source_info_at(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &str,
    ) -> anyhow::Result<SourceRuntime> {
        let state = self.component_observation_at(publication, id, "source")?;
        let (source_type, properties) =
            self.native_component_properties(publication, id, "source")?;
        Ok(SourceRuntime {
            id: id.into(),
            source_type,
            properties,
            status: events::observed_status(state, ComponentStatus::Added),
            error_message: state
                .failure
                .as_ref()
                .map(|failure| format!("{:#}", failure.cause)),
        })
    }

    fn native_reaction_info_at(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &str,
    ) -> anyhow::Result<ReactionRuntime> {
        let state = self.component_observation_at(publication, id, "reaction")?;
        let (reaction_type, properties) =
            self.native_component_properties(publication, id, "reaction")?;
        let queries = publication
            .desired
            .data_dependencies(&ComponentId::try_new(id)?)
            .into_iter()
            .filter(|id| {
                publication
                    .desired
                    .nodes
                    .iter()
                    .any(|node| node.descriptor.id() == id && Self::is_query_node(node))
            })
            .map(|id| id.to_string())
            .collect();
        Ok(ReactionRuntime {
            id: id.into(),
            reaction_type,
            properties,
            queries,
            status: events::observed_status(state, ComponentStatus::Added),
            error_message: state
                .failure
                .as_ref()
                .map(|failure| format!("{:#}", failure.cause)),
        })
    }

    pub(crate) async fn source_schema(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<crate::schema::SourceSchema>> {
        let publication = self.control()?.registry_snapshot();
        self.component_observation_at(&publication, id, "source")?;
        Ok(self
            .records_at(&publication)
            .await?
            .get(id)
            .and_then(|record| {
                if let Value::Source(source) = &record.value {
                    source.source.describe_schema()
                } else {
                    None
                }
            }))
    }

    pub(crate) async fn query_configurations(&self) -> anyhow::Result<Vec<QueryConfig>> {
        let publication = self.control()?.registry_snapshot();
        let mut configs: Vec<_> = self
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
            .collect();
        for node in publication
            .desired
            .nodes
            .iter()
            .filter(|node| Self::is_query_node(node))
        {
            if !configs
                .iter()
                .any(|config| config.id == node.descriptor.id().as_str())
            {
                configs.push(
                    self.native_query_configuration_at(
                        &publication,
                        node.descriptor.id().as_str(),
                    )?,
                );
            }
        }
        configs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(configs)
    }

    pub(crate) async fn query_configuration(&self, id: &str) -> anyhow::Result<QueryConfig> {
        let publication = self.control()?.registry_snapshot();
        if let Some(record) = self.records_at(&publication).await?.get(id) {
            if let Value::Query(query) = &record.value {
                let mut config = query.config.clone();
                config.auto_start = publication.desired.lifecycle_policies[&record.node].auto_start;
                return Ok(config);
            }
        }
        self.native_query_configuration_at(&publication, id)
    }

    pub(crate) async fn subscribe_logs(
        &self,
        id: &str,
        kind: &str,
    ) -> anyhow::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        self.component_status(
            id,
            match kind {
                "query" => "query",
                "source" => "source",
                "reaction" => "reaction",
                _ => anyhow::bail!("unsupported component kind {kind}"),
            },
        )
        .await?;
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
        self.component_status(
            id,
            match kind {
                "query" => "query",
                "source" => "source",
                "reaction" => "reaction",
                _ => anyhow::bail!("unsupported component kind {kind}"),
            },
        )
        .await?;
        Ok(self.events.subscribe(id))
    }

    pub(crate) async fn configuration_snapshot(
        &self,
    ) -> anyhow::Result<crate::config::snapshot::ConfigurationSnapshot> {
        let publication = self.control()?.registry_snapshot();
        let mut snapshot = self.configuration_snapshot_at(&publication).await?;
        for node in publication.desired.nodes.iter() {
            let id = node.descriptor.id().as_str();
            match Self::public_component_kind(&publication.desired, node) {
                Some("source") if !snapshot.sources.iter().any(|source| source.id == id) => {
                    let info = self.native_source_info_at(&publication, id)?;
                    snapshot.sources.push(crate::config::SourceSnapshot {
                        id: id.into(),
                        source_type: info.source_type,
                        status: info.status,
                        auto_start: publication.desired.lifecycle_policies[node.descriptor.id()]
                            .auto_start,
                        properties: info.properties,
                        bootstrap_provider: None,
                    });
                    snapshot
                        .edges
                        .extend(super::snapshot::ownership_edges(&self.config.id, id));
                }
                Some("reaction")
                    if !snapshot.reactions.iter().any(|reaction| reaction.id == id) =>
                {
                    let info = self.native_reaction_info_at(&publication, id)?;
                    snapshot.reactions.push(crate::config::ReactionSnapshot {
                        id: id.into(),
                        reaction_type: info.reaction_type,
                        status: info.status,
                        auto_start: publication.desired.lifecycle_policies[node.descriptor.id()]
                            .auto_start,
                        properties: info.properties,
                        queries: info.queries,
                    });
                    snapshot
                        .edges
                        .extend(super::snapshot::ownership_edges(&self.config.id, id));
                }
                _ => {}
            }
        }
        for node in publication
            .desired
            .nodes
            .iter()
            .filter(|node| Self::is_query_node(node))
        {
            let id = node.descriptor.id().as_str();
            if !snapshot.queries.iter().any(|query| query.id == id) {
                snapshot.queries.push(crate::config::QuerySnapshot {
                    id: id.to_owned(),
                    config: self.native_query_configuration_at(&publication, id)?,
                    status: events::observed_status(
                        self.query_observation_at(&publication, id)?,
                        ComponentStatus::Added,
                    ),
                });
                snapshot
                    .edges
                    .extend(super::snapshot::ownership_edges(&self.config.id, id));
            }
        }
        Ok(snapshot)
    }

    pub(crate) async fn configuration_snapshot_at(
        &self,
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<crate::config::snapshot::ConfigurationSnapshot> {
        use crate::{
            component_graph::{GraphEdge, RelationshipKind},
            config::snapshot::{
                ConfigurationSnapshot, QuerySnapshot, ReactionSnapshot, SourceSnapshot,
            },
        };
        let records = self.records_at(publication).await?;
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
