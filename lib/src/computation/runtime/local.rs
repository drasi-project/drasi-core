// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::management::{OrdinaryComponent, OrdinaryComponentRecord, ProcessLocalComponentKind};

pub(crate) enum LocalConstructor {
    Source(source_creation::SourceConstructor),
    Reaction(ReactionConstructor),
    SourceObject(Box<dyn Source>),
    ReactionObject(Box<dyn Reaction>),
}

pub(crate) struct LocalDefinition {
    pub(crate) name: ComponentName,
    pub(crate) id: Option<ComponentId>,
    pub(crate) auto_start: bool,
    pub(crate) inputs: Vec<crate::config::ComponentReference>,
    pub(crate) constructor: LocalConstructor,
    pub(crate) metadata: HashMap<String, String>,
    pub(crate) bootstrap: Option<crate::config::BootstrapSnapshot>,
}

impl Runtime {
    pub(super) fn retains_local_binding(
        publication: &GraphRegistrySnapshot,
        component: &ComponentId,
        binding: &ResourceId,
    ) -> bool {
        publication
            .desired
            .specifications
            .get(component)
            .is_some_and(|specification| {
                specification
                    .dependencies
                    .get("instance")
                    .is_some_and(|resources| resources.contains(binding))
            })
    }

    pub(crate) fn validate_local_records(
        &self,
        desired: &crate::management::DesiredInstance,
        provided: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        let publication = self.control()?.registry_snapshot();
        for definition in &desired.ordinary {
            if let OrdinaryComponent::ProcessLocal { kind, binding, .. } = &definition.component {
                let resource = match provided.get(binding) {
                    Some(resource) => resource.clone(),
                    None => publication.resource(binding).map_err(|error| {
                        anyhow::anyhow!(
                            "process-local component {} cannot be reconstructed: {error}",
                            definition.id
                        )
                    })?,
                };
                let instance = resource.get::<RuntimeInstance>()?;
                let record = instance.record()?;
                anyhow::ensure!(
                    (!instance.cleanup_started()
                        || Self::retains_local_binding(&publication, &definition.id, binding))
                        && record.node == definition.id
                        && record.resource == *binding,
                    "process-local binding is removed or belongs to another component"
                );
                anyhow::ensure!(
                    matches!(
                        (&record.value, kind),
                        (Value::Source(_), ProcessLocalComponentKind::Source)
                            | (Value::Reaction(_), ProcessLocalComponentKind::Reaction)
                    ),
                    "process-local binding has a different component kind"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_local(
        self: &Arc<Self>,
        id: ComponentId,
        definition: LocalDefinition,
    ) -> anyhow::Result<(OrdinaryComponentRecord, ComponentBatch)> {
        let value = match definition.constructor {
            LocalConstructor::Source(constructor) => Value::Source(SourceInstance::deferred(
                source_creation::DeferredSource::new(
                    id.clone(),
                    definition.auto_start,
                    constructor,
                ),
                self.services.clone(),
                self.changed.clone(),
                self.control()?,
                Arc::downgrade(self),
            )),
            LocalConstructor::Reaction(constructor) => Value::Reaction(ReactionInstance::deferred(
                reaction_creation::DeferredReaction::new_committed(
                    id.clone(),
                    definition.inputs.clone(),
                    definition.auto_start,
                    constructor,
                ),
                self,
            )),
            LocalConstructor::SourceObject(source) => {
                anyhow::ensure!(
                    source.id() == &id,
                    "source identity does not match its declaration"
                );
                Value::Source(SourceInstance::new(
                    source,
                    self.services.clone(),
                    self.changed.clone(),
                    self.control()?,
                    Arc::downgrade(self),
                ))
            }
            LocalConstructor::ReactionObject(reaction) => {
                anyhow::ensure!(
                    reaction.id() == &id,
                    "reaction identity does not match its declaration"
                );
                Value::Reaction(ReactionInstance::new(reaction, self))
            }
        };
        let kind = match value {
            Value::Source(_) => ProcessLocalComponentKind::Source,
            Value::Reaction(_) => ProcessLocalComponentKind::Reaction,
        };
        let record = self.new_record(&id, value, definition.metadata, definition.bootstrap)?;
        let definition = OrdinaryComponentRecord {
            id,
            name: definition.name,
            auto_start: definition.auto_start,
            component: OrdinaryComponent::ProcessLocal {
                kind,
                binding: record.resource.clone(),
                inputs: definition.inputs,
            },
        };
        let batch = self.record_batch(&definition, &record)?;
        Ok((definition, batch))
    }

    pub(crate) fn attach_configuration_owner(
        &self,
        owner: Weak<OnceLock<crate::management::Management>>,
    ) -> anyhow::Result<()> {
        self.configuration_owner
            .set(owner)
            .map_err(|_| anyhow::anyhow!("configuration owner already attached"))
    }

    pub(super) fn configuration_owner(
        &self,
    ) -> anyhow::Result<Arc<OnceLock<crate::management::Management>>> {
        self.configuration_owner
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))
    }

    pub(super) async fn accept_local(
        self: &Arc<Self>,
        definition: LocalDefinition,
    ) -> anyhow::Result<ComponentHandle> {
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is not initialized"))?;
        let receipt = owner
            .accept_auto(crate::management::AuthoringOperation::Local {
                definitions: vec![definition],
                queries: Vec::new(),
                native: Vec::new(),
                native_factories: Vec::new(),
            })
            .await?;
        let id = receipt
            .creation
            .as_ref()
            .and_then(|creation| creation.components.first())
            .ok_or_else(|| anyhow::anyhow!("accepted component has no identity assignment"))?
            .id
            .clone();
        owner.wait_declared(&id).await?;
        self.component_handle(&id)
    }

    pub(crate) async fn rename_owned(
        &self,
        id: ComponentId,
        name: ComponentName,
    ) -> anyhow::Result<()> {
        let generation = self.control()?.component_handle_by_id(&id)?.generation();
        self.rename_owned_at(id, generation, name).await
    }

    pub(super) async fn rename_owned_at(
        &self,
        id: ComponentId,
        generation: ComponentGeneration,
        name: ComponentName,
    ) -> anyhow::Result<()> {
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is not initialized"))?;
        let receipt = owner
            .accept_auto(crate::management::AuthoringOperation::Edit {
                generations: BTreeMap::from([(id.clone(), generation)]),
                delete_owned_resources: BTreeSet::new(),
                edit: Box::new(move |desired| {
                    if let Some(record) = desired.ordinary.iter_mut().find(|record| record.id == id)
                    {
                        record.name = name;
                    } else if let Some(record) = desired
                        .topology
                        .components
                        .iter_mut()
                        .find(|record| record.descriptor.id() == &id)
                    {
                        record.name = name;
                    } else {
                        anyhow::bail!("component {id} is not owned by this configuration");
                    }
                    Ok(())
                }),
            })
            .await?;
        let status = owner.wait_applied(receipt.revision).await?;
        anyhow::ensure!(
            status.error.is_none(),
            "rename reconciliation failed: {:?}",
            status.error
        );
        Ok(())
    }

    pub(super) async fn update_owned_query(
        &self,
        id: &ComponentId,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            &config.id == id,
            "replacement query ID does not match '{id}'"
        );
        let publication = self.control()?.registry_snapshot();
        let current = self.native_query_configuration_at(&publication, id)?;
        anyhow::ensure!(
            current.name == config.name,
            "use rename_component to change a component name"
        );
        let id = id.clone();
        let generation = self.control()?.component_handle_by_id(&id)?.generation();
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
        let operation = if owner
            .configuration()?
            .desired
            .ordinary
            .iter()
            .any(|record| record.id == id)
        {
            let record =
                crate::management::OrdinaryComponentDefinition::query(config.into_authoring())?
                    .with_assigned_id(id.clone());
            crate::management::AuthoringOperation::Edit {
                generations: BTreeMap::from([(id.clone(), generation)]),
                delete_owned_resources: BTreeSet::new(),
                edit: Box::new(move |desired| {
                    let current = desired
                        .ordinary
                        .iter_mut()
                        .find(|record| record.id == id)
                        .ok_or_else(|| anyhow::anyhow!("query {id} was removed"))?;
                    *current = record;
                    Ok(())
                }),
            }
        } else {
            crate::management::AuthoringOperation::QueryUpdate {
                id,
                generation,
                configuration: config,
            }
        };
        let receipt = owner.accept_auto(operation).await?;
        let status = owner.wait_applied(receipt.revision).await?;
        anyhow::ensure!(
            status.error.is_none(),
            "query replacement failed: {:?}",
            status.error
        );
        owner.wait_updates(&receipt.updates).await?;
        Ok(())
    }

    /// Route accepted removal to the actual cleanup owner without releasing it early.
    pub(crate) async fn prepare_owned_resource_deletion(
        &self,
        components: &BTreeSet<ComponentId>,
    ) -> anyhow::Result<BTreeSet<ComponentId>> {
        if components.is_empty() {
            return Ok(BTreeSet::new());
        }
        let publication = self.control()?.registry_snapshot();
        let records = self.records_matching_at(&publication, |id| components.contains(id))?;
        let queries = self.ordinary_queries_at(&publication)?;
        let mut native = BTreeSet::new();
        for id in components {
            if let Some(record) = records.get(id) {
                record.owner.remove_data();
            } else if queries.get(id).is_some_and(|declaration| {
                declaration.failure.is_some() || !declaration.waiting.is_empty()
            }) {
                // Failed or unresolved authoring has not allocated query-owned state.
            } else {
                native.insert(id.clone());
            }
        }
        Ok(native)
    }

    pub(super) async fn remove_owned(
        &self,
        id: &ComponentId,
        kind: &str,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        let kind = match kind {
            "source" => "source",
            "query" => "query",
            "reaction" => "reaction",
            _ => anyhow::bail!("unsupported component kind {kind}"),
        };
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let observed = self.component_observation_at(&publication, id, kind)?;
        let id = id.clone();
        let members = if kind == "query" {
            native_query::query_members(&publication, &id)
        } else {
            BTreeMap::from([(id.clone(), observed.generation)])
        };
        let selected: BTreeSet<_> = members.keys().cloned().collect();
        let outlet = QueryResultsOutletFactory::default()
            .descriptor()
            .implementation
            .clone();
        let dependents: BTreeSet<_> = selected
            .iter()
            .flat_map(|id| publication.desired.data_dependents(id))
            .filter(|id| !selected.contains(id))
            .filter(|id| {
                kind != "query"
                    || publication
                        .desired
                        .specifications
                        .get(id)
                        .is_none_or(|spec| spec.implementation != outlet)
            })
            .collect();
        anyhow::ensure!(
            dependents.is_empty(),
            "Depended on by: {}",
            dependents
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        let candidates = publication
            .desired
            .select(GraphSelection::Exact(selected.iter().cloned().collect()))?
            .resources;
        let infrastructure = self.infrastructure_resources(&publication, &BTreeSet::new())?;
        let log_key = self.component_log_key(&publication, &id)?;
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
        let receipt =
            owner
                .accept_auto(crate::management::AuthoringOperation::Edit {
                    generations: members,
                    delete_owned_resources: if cleanup {
                        BTreeSet::from([id.clone()])
                    } else {
                        BTreeSet::new()
                    },
                    edit: Box::new(move |desired| {
                        let before = desired.ordinary.len() + desired.topology.components.len();
                        desired
                            .ordinary
                            .retain(|record| !selected.contains(&record.id));
                        desired
                            .topology
                            .components
                            .retain(|node| !selected.contains(node.descriptor.id()));
                        anyhow::ensure!(
                            before != desired.ordinary.len() + desired.topology.components.len(),
                            "component {id} is not owned by this configuration"
                        );
                        desired.topology.relationships.retain(|edge| {
                            !selected.contains(&edge.definition.from.component)
                                && !selected.contains(&edge.definition.to.component)
                        });
                        desired.topology.boundary_relationships.retain(|edge| {
                            !selected.contains(&edge.definition.from.component)
                                && !selected.contains(&edge.definition.to.component)
                        });
                        desired.topology.subscriptions.retain(|(from, to)| {
                            !selected.contains(from) && !selected.contains(to)
                        });
                        desired.topology.control_connections.retain(|(from, to)| {
                            !selected.contains(from) && !selected.contains(to)
                        });
                        desired
                            .topology
                            .component_resources
                            .retain(|id, _| !selected.contains(id));
                        desired
                            .topology
                            .component_plugins
                            .retain(|id, _| !selected.contains(id));
                        desired
                            .topology
                            .readiness_required
                            .retain(|id| !selected.contains(id));
                        desired
                            .topology
                            .recovery_requirements
                            .retain(|requirement| !selected.contains(&requirement.consumer));
                        native_query::remove_unused_resources(
                            &mut desired.topology,
                            candidates,
                            &infrastructure,
                        )?;
                        Ok(())
                    }),
                })
                .await?;
        let status = owner.wait_applied(receipt.revision).await?;
        anyhow::ensure!(
            status.error.is_none() && status.resource_errors.is_empty(),
            "component removal is incomplete: {status:?}"
        );
        self.logs.remove_component_by_key(&log_key).await;
        self.notify();
        Ok(())
    }
}
