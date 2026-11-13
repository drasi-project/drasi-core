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
    observability::{ComponentLogKey, ComponentLogType, LogMessage},
};

pub(super) fn status(record: &Record, observed: &ObservedComponent) -> ComponentStatus {
    events::observed_status(observed, record.initial_status)
}

fn error_message(record: &Record, observed: &ObservedComponent) -> Option<String> {
    if status(record, observed) != ComponentStatus::Error {
        return observed.waiting_reason();
    }
    observed
        .failure
        .as_ref()
        .map(|failure| format!("{:#}", failure.cause))
}

#[cfg(test)]
pub(super) struct SourceSchemaCaptureTest {
    pub component: ComponentId,
    pub release: Arc<tokio::sync::Notify>,
    pub observations: Arc<std::sync::Mutex<Vec<SourceSchemaCaptureObservation>>>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct SourceSchemaCaptureObservation {
    pub stage: &'static str,
    pub captured: Option<ObservedComponent>,
    pub current: Option<ObservedComponent>,
    pub captured_reader: std::result::Result<bool, String>,
    pub current_reader: std::result::Result<bool, String>,
}

impl Runtime {
    #[cfg(test)]
    fn record_source_schema_capture_test(
        &self,
        stage: &'static str,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) {
        let test = self
            .source_schema_capture_test
            .lock()
            .expect("source schema test hook lock")
            .clone();
        let Some(test) = test.filter(|test| &test.component == id) else {
            return;
        };
        let current = self.control().map(|control| control.registry_snapshot());
        let (current, current_reader) = match current {
            Ok(current) => (
                current.observed.components.get(id).cloned(),
                current
                    .source_schema_reader(id)
                    .map(|reader| reader.is_some())
                    .map_err(|error| format!("{error:?}")),
            ),
            Err(error) => (None, Err(format!("{error:#}"))),
        };
        test.observations
            .lock()
            .expect("source schema test observation lock")
            .push(SourceSchemaCaptureObservation {
                stage,
                captured: publication.observed.components.get(id).cloned(),
                current,
                captured_reader: publication
                    .source_schema_reader(id)
                    .map(|reader| reader.is_some())
                    .map_err(|error| format!("{error:?}")),
                current_reader,
            });
    }

    #[cfg(test)]
    async fn pause_source_schema_capture_test(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) {
        let test = self
            .source_schema_capture_test
            .lock()
            .expect("source schema test hook lock")
            .clone();
        if let Some(test) = test.filter(|test| &test.component == id) {
            self.record_source_schema_capture_test("fresh-capture", publication, id);
            test.release.notified().await;
        }
    }

    pub(crate) async fn query_inspector(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<ComputationInspector> {
        let publication = self.control()?.registry_snapshot();
        self.query_observation_at(&publication, id)?;
        self.inspector()
    }

    pub(super) fn is_query_node(node: &NodeSnapshot) -> bool {
        node.descriptor
            .semantic_kind()
            .unwrap_or_else(|| node.role.into())
            == ComponentSemanticKind::Query
    }

    pub(crate) fn public_component_kind(
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
        id: &ComponentId,
        kind: &'static str,
    ) -> anyhow::Result<&'a ObservedComponent> {
        let node = publication
            .desired
            .nodes
            .iter()
            .find(|node| {
                node.descriptor.id() == id
                    && Self::public_component_kind(&publication.desired, node) == Some(kind)
            })
            .ok_or_else(|| crate::error::ComponentNotFoundError::new(kind, id.clone()))?;
        publication
            .observed
            .components
            .get(node.descriptor.id())
            .ok_or_else(|| anyhow::anyhow!("{kind} '{id}' has no graph observation"))
    }

    fn native_component_properties(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
        kind: &'static str,
    ) -> anyhow::Result<(String, HashMap<String, serde_json::Value>)> {
        self.component_observation_at(publication, id, kind)?;
        let component = id.clone();
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
        id: &ComponentId,
    ) -> anyhow::Result<QueryConfig> {
        let node = publication
            .desired
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == id && Self::is_query_node(node))
            .ok_or_else(|| crate::error::ComponentNotFoundError::new("query", id.clone()))?;
        let component = node.descriptor.id();
        let mut config = if let Some(ConfigurationValue::Literal(value)) = publication
            .desired
            .specifications
            .get(component)
            .and_then(|spec| spec.configuration.get("query_config"))
        {
            serde_json::from_value(value.clone())?
        } else if let Some(config) = publication
            .desired
            .specifications
            .get(component)
            .and_then(|spec| publication.authored_query_configuration(spec))
        {
            config?.into_assigned(
                node.name.clone(),
                publication.desired.lifecycle_policies[component].auto_start,
            )
        } else if let Some(api) = publication.query_api(component) {
            api.config.clone().into_assigned(
                node.name.clone(),
                publication.desired.lifecycle_policies[component].auto_start,
            )
        } else {
            anyhow::bail!("Query '{id}' exists but does not expose ordinary query configuration in its current construction state");
        };
        anyhow::ensure!(
            &config.id == id,
            "query configuration identity does not match its graph node"
        );
        config.auto_start = publication.desired.lifecycle_policies[component].auto_start;
        config.name = node.name.clone();
        Ok(config)
    }

    pub(super) fn query_observation_at<'a>(
        &self,
        publication: &'a GraphRegistrySnapshot,
        id: &ComponentId,
    ) -> anyhow::Result<&'a ObservedComponent> {
        self.component_observation_at(publication, id, "query")
    }

    pub(crate) async fn component_events(&self, id: &ComponentId) -> Vec<ComponentEvent> {
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
                    .is_none_or(|kind| kind == &event.component_type)
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
        for (id, declaration) in self.ordinary_queries_at(&publication)? {
            let hosts = if let Some(resources) = publication.desired.specifications[&id]
                .dependencies
                .get("source_hosts")
            {
                resources
                    .iter()
                    .filter(|resource| {
                        publication
                            .observed
                            .resources
                            .get(*resource)
                            .is_some_and(|state| state.realization == ResourceRealization::Created)
                    })
                    .map(|resource| publication.resource(resource)?.get::<SourcePluginHost>())
                    .collect::<GraphResult<Vec<_>>>()?
            } else {
                declaration.sources.clone()
            };
            for host in &hosts {
                let source = host.source()?;
                let Some(current) = records.get(source.id()) else {
                    continue;
                };
                if !matches!(&current.value, Value::Source(current) if Arc::ptr_eq(&current.source, &source))
                {
                    continue;
                }
                inventory.links.insert(ScopedGraphEntityLink {
                    from: root.entity(GraphEntityId::Resource(
                        ComputationPipelineBuilder::source_resource_id(source.id())?,
                    )),
                    to: root.entity(GraphEntityId::Component(current.node.clone())),
                    kind: GraphEntityLinkKind::UsesComponent,
                });
            }
            if let Some(providers) = publication.desired.specifications[&id]
                .dependencies
                .get("backend")
            {
                for provider in providers {
                    inventory.links.insert(ScopedGraphEntityLink {
                        from: root.entity(GraphEntityId::Resource(
                            ComputationPipelineBuilder::index_resource_id(&id)?,
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
        id: &ComponentId,
        kind: &'static str,
    ) -> anyhow::Result<(Record, ObservedComponent)> {
        let publication = self.control()?.registry_snapshot();
        let record = self
            .records_at(&publication)
            .await?
            .remove(id)
            .filter(|record| record.value.instance().kind() == kind)
            .ok_or_else(|| crate::error::ComponentNotFoundError::new(kind, id.clone()))?;
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
    ) -> anyhow::Result<Vec<crate::config::ComponentListEntry>> {
        let publication = self.control()?.registry_snapshot();
        let records = self.records_at(&publication).await?;
        let mut components: Vec<_> = publication
            .desired
            .nodes
            .iter()
            .filter(|node| Self::public_component_kind(&publication.desired, node) == Some(kind))
            .map(|node| {
                let id = node.descriptor.id();
                let state = publication
                    .observed
                    .components
                    .get(node.descriptor.id())
                    .ok_or_else(|| anyhow::anyhow!("{kind} '{id}' has no graph observation"))?;
                Ok(crate::config::ComponentListEntry {
                    id: id.clone(),
                    name: node.name.clone(),
                    status: records.get(id).filter(|_| kind != "query").map_or_else(
                        || events::observed_status(state, ComponentStatus::Added),
                        |record| status(record, state),
                    ),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        components.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(components)
    }

    pub(crate) async fn component_status(
        &self,
        id: &ComponentId,
        kind: &'static str,
    ) -> anyhow::Result<ComponentStatus> {
        let publication = self.control()?.registry_snapshot();
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
        Ok(events::observed_status(
            self.component_observation_at(&publication, id, kind)?,
            ComponentStatus::Added,
        ))
    }

    pub(crate) async fn source_info(&self, id: &ComponentId) -> anyhow::Result<SourceRuntime> {
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
            name: publication
                .desired
                .component_by_id(&record.node)
                .ok_or(GraphError::StaleGeneration)?
                .name
                .clone(),
            source_type: source.source.type_name().to_owned(),
            status: status(&record, &observed),
            error_message: error_message(&record, &observed),
            properties: source.source.properties(),
        })
    }

    pub(crate) async fn query_info(&self, id: &ComponentId) -> anyhow::Result<QueryRuntime> {
        let publication = self.control()?.registry_snapshot();
        let config = self.native_query_configuration_at(&publication, id)?;
        let observed = self.query_observation_at(&publication, id)?;
        Ok(QueryRuntime {
            id: id.to_owned(),
            name: config.name,
            query: config.query,
            status: events::observed_status(observed, ComponentStatus::Added),
            error_message: observed
                .failure
                .as_ref()
                .map(|failure| format!("{:#}", failure.cause))
                .or_else(|| observed.waiting_reason()),
            inputs: config.inputs,
            joins: config.joins,
        })
    }

    pub(crate) async fn reaction_info(&self, id: &ComponentId) -> anyhow::Result<ReactionRuntime> {
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
            name: publication
                .desired
                .component_by_id(&record.node)
                .ok_or(GraphError::StaleGeneration)?
                .name
                .clone(),
            reaction_type: reaction.reaction.type_name().to_owned(),
            status: status(&record, &observed),
            error_message: error_message(&record, &observed),
            queries: reaction.references()?,
            properties: reaction.reaction.properties(),
        })
    }

    fn native_source_info_at(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) -> anyhow::Result<SourceRuntime> {
        let state = self.component_observation_at(publication, id, "source")?;
        let (source_type, properties) =
            self.native_component_properties(publication, id, "source")?;
        Ok(SourceRuntime {
            id: id.clone(),
            name: publication
                .desired
                .component_by_id(&id.clone())
                .ok_or(GraphError::StaleGeneration)?
                .name
                .clone(),
            source_type,
            properties,
            status: events::observed_status(state, ComponentStatus::Added),
            error_message: state
                .failure
                .as_ref()
                .map(|failure| format!("{:#}", failure.cause))
                .or_else(|| state.waiting_reason()),
        })
    }

    fn native_reaction_info_at(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) -> anyhow::Result<ReactionRuntime> {
        let state = self.component_observation_at(publication, id, "reaction")?;
        let (reaction_type, properties) =
            self.native_component_properties(publication, id, "reaction")?;
        let queries = publication
            .desired
            .data_dependencies(&id.clone())
            .into_iter()
            .filter(|id| {
                publication
                    .desired
                    .nodes
                    .iter()
                    .any(|node| node.descriptor.id() == id && Self::is_query_node(node))
            })
            .map(crate::config::ComponentReference::by_id)
            .collect();
        Ok(ReactionRuntime {
            id: id.clone(),
            name: publication
                .desired
                .component_by_id(&id.clone())
                .ok_or(GraphError::StaleGeneration)?
                .name
                .clone(),
            reaction_type,
            properties,
            queries,
            status: events::observed_status(state, ComponentStatus::Added),
            error_message: state
                .failure
                .as_ref()
                .map(|failure| format!("{:#}", failure.cause))
                .or_else(|| state.waiting_reason()),
        })
    }

    pub(crate) async fn source_schema(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<Option<crate::schema::SourceSchema>> {
        let publication = self.source_schema_publication(Some(id)).await?;
        #[cfg(test)]
        self.pause_source_schema_capture_test(&publication, id)
            .await;
        self.source_schema_at(&publication, id).await
    }

    pub(super) async fn source_schema_publication(
        &self,
        selected: Option<&ComponentId>,
    ) -> anyhow::Result<Arc<GraphRegistrySnapshot>> {
        let control = self.control()?;
        let mut publications = control.subscribe_registry();
        let mut states = control.subscribe();
        let mut generations = BTreeMap::new();
        let mut settled_operations = BTreeMap::new();
        let mut construction_operations = BTreeMap::new();
        loop {
            let state = *states.borrow_and_update();
            anyhow::ensure!(
                !matches!(
                    state,
                    GraphState::Stopping
                        | GraphState::Completed
                        | GraphState::Cancelled
                        | GraphState::Failed
                        | GraphState::CleanupRequired
                ),
                GraphError::StaleGeneration
            );
            let publication = publications.borrow_and_update().clone();
            for (id, generation) in &generations {
                let current = self
                    .component_observation_at(&publication, id, "source")
                    .map_err(|_| GraphError::StaleGeneration)?;
                anyhow::ensure!(
                    current.generation == *generation,
                    GraphError::StaleGeneration
                );
            }
            let sources: Vec<_> = match selected {
                Some(id) => vec![id.clone()],
                None => publication
                    .desired
                    .nodes
                    .iter()
                    .filter(|node| {
                        Self::public_component_kind(&publication.desired, node) == Some("source")
                    })
                    .map(|node| node.descriptor.id().clone())
                    .collect(),
            };
            let mut pending = false;
            for id in sources {
                let observed = self.component_observation_at(&publication, &id, "source")?;
                generations.entry(id.clone()).or_insert(observed.generation);
                if let Some(failure) = &observed.failure {
                    anyhow::bail!(GraphError::Reported {
                        cause: failure.cause.clone(),
                    });
                }
                anyhow::ensure!(
                    observed.lifecycle != ComponentLifecycle::Stopping,
                    GraphError::StaleGeneration
                );
                if let Some(operation) = settled_operations.get(&id) {
                    anyhow::ensure!(
                        observed.operation == *operation,
                        GraphError::StaleGeneration
                    );
                }
                let admission = publication.source_schema_admission(&id);
                if let Some(operation) = construction_operations.get(&id) {
                    anyhow::ensure!(
                        observed.operation == *operation
                            && admission.is_some_and(|(reservation, _)| {
                                reservation == Some(*operation)
                            }),
                        GraphError::StaleGeneration
                    );
                }
                if observed.realization == RealizationState::CreationFailed {
                    anyhow::bail!(GraphError::CreationStopped { component: id });
                }
                if observed.realization == RealizationState::Pending {
                    anyhow::ensure!(
                        !observed.lifecycle_requested
                            || observed.lifecycle != ComponentLifecycle::Stopped,
                        GraphError::CreationStopped { component: id }
                    );
                    pending = true;
                    continue;
                }
                let Some((reservation, captured)) = admission else {
                    anyhow::bail!(GraphError::Creation {
                        component: id,
                        disposition: FailureDisposition::Retryable,
                        source: anyhow::anyhow!(
                            "source construction admission has not been published"
                        ),
                    });
                };
                if captured && reservation.is_some() {
                    settled_operations
                        .entry(id.clone())
                        .or_insert(observed.operation);
                    continue;
                }
                if observed.realization == RealizationState::Blocked {
                    anyhow::bail!(GraphError::Creation {
                        component: id,
                        disposition: FailureDisposition::Retryable,
                        source: anyhow::anyhow!(
                            "source schema capture is blocked by construction prerequisites: {:?}",
                            observed.waiting_for
                        ),
                    });
                }
                if observed.lifecycle_requested && observed.lifecycle == ComponentLifecycle::Stopped
                {
                    anyhow::bail!(GraphError::CreationStopped { component: id });
                }
                let Some(reservation) = reservation else {
                    anyhow::bail!(GraphError::CreationStopped { component: id });
                };
                anyhow::ensure!(
                    observed.realization == RealizationState::Creating
                        && observed.operation == reservation,
                    GraphError::StaleGeneration
                );
                construction_operations.entry(id).or_insert(reservation);
                pending = true;
            }
            if !pending {
                return Ok(publication);
            }
            tokio::select! {
                changed = publications.changed() => {
                    changed.map_err(|_| GraphError::ControllerClosed)?;
                }
                changed = states.changed() => {
                    changed.map_err(|_| GraphError::ControllerClosed)?;
                }
            }
        }
    }

    pub(super) fn ensure_source_schema_current(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) -> anyhow::Result<()> {
        let expected = self.component_observation_at(publication, id, "source")?;
        let control = self.control()?;
        let current = control.registry_snapshot();
        let observed = self
            .component_observation_at(&current, id, "source")
            .map_err(|_| GraphError::StaleGeneration)?;
        #[cfg(test)]
        if observed.generation != expected.generation || observed.operation != expected.operation {
            self.record_source_schema_capture_test("currency-rejected", publication, id);
        }
        anyhow::ensure!(
            observed.generation == expected.generation && observed.operation == expected.operation,
            GraphError::StaleGeneration
        );
        let component = id.clone();
        let reader = publication.source_schema_reader(&component)?;
        let current_reader = current.source_schema_reader(&component)?;
        anyhow::ensure!(
            match (&reader, &current_reader) {
                (Some(reader), Some(current)) => Arc::ptr_eq(reader, current),
                (None, None) => true,
                _ => false,
            },
            GraphError::StaleGeneration
        );
        if reader.is_some() {
            anyhow::ensure!(
                observed.lifecycle != ComponentLifecycle::Stopping
                    && !matches!(
                        control.state(),
                        GraphState::Stopping
                            | GraphState::Completed
                            | GraphState::Cancelled
                            | GraphState::Failed
                            | GraphState::CleanupRequired
                    ),
                GraphError::StaleGeneration
            );
        }
        Ok(())
    }

    pub(super) async fn source_schema_at(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) -> anyhow::Result<Option<crate::schema::SourceSchema>> {
        self.component_observation_at(publication, id, "source")?;
        let control = self.control()?;
        let mut publications = control.subscribe_registry();
        if let Some(reader) = publication.source_schema_reader(&id.clone())? {
            let read = reader.describe_schema();
            tokio::pin!(read);
            loop {
                self.ensure_source_schema_current(publication, id)?;
                tokio::select! {
                    result = &mut read => {
                        self.ensure_source_schema_current(publication, id)?;
                        return result;
                    }
                    result = publications.changed() => {
                        result.map_err(|_| GraphError::StaleGeneration)?;
                    }
                }
            }
        }
        #[cfg(test)]
        self.record_source_schema_capture_test("ordinary-fallback-entered", publication, id);
        let schema = self
            .records_at(publication)
            .await?
            .get(id)
            .and_then(|record| {
                if let Value::Source(source) = &record.value {
                    source.source.describe_schema()
                } else {
                    None
                }
            });
        #[cfg(test)]
        self.record_source_schema_capture_test("ordinary-fallback-completed", publication, id);
        self.ensure_source_schema_current(publication, id)?;
        Ok(schema)
    }

    pub(crate) async fn query_configurations(&self) -> anyhow::Result<Vec<QueryConfig>> {
        let publication = self.control()?.registry_snapshot();
        self.query_configurations_at(&publication)
    }

    pub(super) fn query_configurations_at(
        &self,
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<Vec<QueryConfig>> {
        let mut configs: Vec<QueryConfig> = Vec::new();
        for node in publication
            .desired
            .nodes
            .iter()
            .filter(|node| Self::is_query_node(node))
        {
            if !configs
                .iter()
                .any(|config| &config.id == node.descriptor.id())
            {
                configs
                    .push(self.native_query_configuration_at(publication, node.descriptor.id())?);
            }
        }
        configs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(configs)
    }

    pub(crate) async fn query_configuration(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<QueryConfig> {
        let publication = self.control()?.registry_snapshot();
        self.native_query_configuration_at(&publication, id)
    }

    pub(crate) fn component_log_key(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
    ) -> anyhow::Result<ComponentLogKey> {
        let node = publication
            .desired
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == id)
            .ok_or_else(|| crate::error::ComponentNotFoundError::new("component", id.clone()))?;
        Ok(ComponentLogKey::new(
            &self.config.id,
            ComponentLogType::for_component(node.role, node.descriptor.semantic_kind()),
            id.clone(),
        ))
    }

    pub(crate) async fn subscribe_logs(
        &self,
        id: &ComponentId,
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
        let publication = self.control()?.registry_snapshot();
        let key = self.component_log_key(&publication, id)?;
        Ok(self.logs.subscribe_by_key(&key).await)
    }

    pub(crate) async fn subscribe_events(
        &self,
        id: &ComponentId,
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
            let id = node.descriptor.id();
            match Self::public_component_kind(&publication.desired, node) {
                Some("source") if !snapshot.sources.iter().any(|source| &source.id == id) => {
                    let info = self.native_source_info_at(&publication, id)?;
                    snapshot.sources.push(crate::config::SourceSnapshot {
                        id: id.clone(),
                        name: node.name.clone(),
                        source_type: info.source_type,
                        status: info.status,
                        auto_start: publication.desired.lifecycle_policies[node.descriptor.id()]
                            .auto_start,
                        properties: info.properties,
                        bootstrap_provider: None,
                    });
                    snapshot.edges.extend(super::snapshot::ownership_edges(
                        &self.config.id,
                        &GraphEntityId::Component(id.clone()),
                    ));
                }
                Some("reaction")
                    if !snapshot.reactions.iter().any(|reaction| &reaction.id == id) =>
                {
                    let info = self.native_reaction_info_at(&publication, id)?;
                    snapshot.reactions.push(crate::config::ReactionSnapshot {
                        id: id.clone(),
                        name: node.name.clone(),
                        reaction_type: info.reaction_type,
                        status: info.status,
                        auto_start: publication.desired.lifecycle_policies[node.descriptor.id()]
                            .auto_start,
                        properties: info.properties,
                        queries: info.queries,
                    });
                    snapshot.edges.extend(super::snapshot::ownership_edges(
                        &self.config.id,
                        &GraphEntityId::Component(id.clone()),
                    ));
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
            let id = node.descriptor.id();
            if !snapshot.queries.iter().any(|query| &query.id == id) {
                snapshot
                    .queries
                    .push(crate::config::QueryConfigurationSnapshot {
                        id: id.to_owned(),
                        name: node.name.clone(),
                        config: self.native_query_configuration_at(&publication, id)?,
                        status: events::observed_status(
                            self.query_observation_at(&publication, id)?,
                            ComponentStatus::Added,
                        ),
                    });
                snapshot.edges.extend(super::snapshot::ownership_edges(
                    &self.config.id,
                    &GraphEntityId::Component(id.clone()),
                ));
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
                ConfigurationSnapshot, QueryConfigurationSnapshot, ReactionSnapshot, SourceSnapshot,
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
            if !record.value.instance().configuration_available() {
                return Err(crate::config::ComponentConfigurationUnavailable {
                    component: record.node.clone(),
                }
                .into());
            }
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
                        name: publication
                            .desired
                            .component_by_id(&record.node)
                            .ok_or(GraphError::StaleGeneration)?
                            .name
                            .clone(),
                        source_type: source.source.type_name().to_owned(),
                        status,
                        auto_start,
                        properties: source.source.properties(),
                        bootstrap_provider: record.bootstrap_recipe.clone(),
                    });
                }
                Value::Reaction(reaction) => snapshot.reactions.push(ReactionSnapshot {
                    id: id.clone(),
                    name: publication
                        .desired
                        .component_by_id(&record.node)
                        .ok_or(GraphError::StaleGeneration)?
                        .name
                        .clone(),
                    reaction_type: reaction.reaction.type_name().to_owned(),
                    status,
                    auto_start,
                    queries: reaction.references()?,
                    properties: reaction.reaction.properties(),
                }),
            }
            snapshot.edges.extend(super::snapshot::ownership_edges(
                &self.config.id,
                &GraphEntityId::Component(id.clone()),
            ));
        }
        let ordinary_queries = self.ordinary_queries_at(publication)?;
        for id in ordinary_queries.keys() {
            let mut config = self.native_query_configuration_at(publication, id)?;
            config.auto_start = publication.desired.lifecycle_policies[id].auto_start;
            snapshot.queries.push(QueryConfigurationSnapshot {
                id: id.clone(),
                name: config.name.clone(),
                config,
                status: events::observed_status(
                    self.query_observation_at(publication, id)?,
                    ComponentStatus::Added,
                ),
            });
            snapshot.edges.extend(super::snapshot::ownership_edges(
                &self.config.id,
                &GraphEntityId::Component(id.clone()),
            ));
        }
        for (from, to) in publication.desired.subscriptions.iter() {
            snapshot.edges.push(GraphEdge {
                from: GraphEntityId::Component(from.clone()),
                to: GraphEntityId::Component(to.clone()),
                relationship: RelationshipKind::Feeds,
            });
            if (matches!(
                records.get(from).map(|record| &record.value),
                Some(Value::Source(_))
            ) && ordinary_queries.contains_key(to))
                || (ordinary_queries.contains_key(from)
                    && matches!(
                        records.get(to).map(|record| &record.value),
                        Some(Value::Reaction(_))
                    ))
            {
                snapshot.edges.push(GraphEdge {
                    from: GraphEntityId::Component(to.clone()),
                    to: GraphEntityId::Component(from.clone()),
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
