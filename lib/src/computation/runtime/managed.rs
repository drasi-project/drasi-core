// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::management::{OrdinaryComponent, OrdinaryComponentRecord, OrdinaryComponentResolver};

pub(super) struct ManagedQueryContext<'a> {
    pub(super) record: &'a OrdinaryComponentRecord,
    pub(super) sources: &'a BTreeMap<ComponentId, Arc<SourcePluginHost>>,
    pub(super) components: &'a BTreeMap<ComponentId, QueryInputDeclaration>,
}

pub(super) struct QueryInputDeclaration {
    pub(super) descriptor: ComponentDescriptor,
    pub(super) primary_ports: PortPrimaries,
}

impl QueryInputDeclaration {
    fn from_component(component: &DesiredComponent, factories: &FactoryRegistry) -> Self {
        let primary_ports = match &component.construction {
            ComponentConstruction::Factory(spec) => factories
                .get(&spec.implementation)
                .map(|factory| factory.primary_ports())
                .unwrap_or_default(),
            ComponentConstruction::External { .. } => PortPrimaries::default(),
        };
        Self {
            descriptor: component.descriptor.clone(),
            primary_ports,
        }
    }
}

impl Runtime {
    /// Compile committed recipes into the existing graph's declarations and
    /// resources. This does not invoke Host constructors or grant public admission.
    pub(crate) async fn managed_components(
        self: &Arc<Self>,
        records: &[OrdinaryComponentRecord],
        native: &DesiredTopology,
        previous: &DesiredTopology,
        factories: &FactoryRegistry,
        resolver: Arc<dyn OrdinaryComponentResolver>,
        pending: &BTreeMap<ResourceId, ResourceHandle>,
        updates: &[crate::management::AcceptedComponentUpdate],
    ) -> anyhow::Result<ComponentBatch> {
        let publication = self.control()?.registry_snapshot();
        let requested: BTreeSet<_> = records.iter().map(|record| record.id.clone()).collect();
        // Removed members may be awaiting cleanup with their resource binding already withdrawn.
        let current = self.records_matching_at(&publication, |id| requested.contains(id))?;
        let queries = self.ordinary_queries_at(&publication)?;
        let mut batch = ComponentBatch {
            definition: crate::management::DesiredInstance::default().topology,
            bindings: TopologyBindings::default(),
        };
        let mut sources = BTreeMap::new();
        let mut projected: BTreeMap<_, _> = native
            .components
            .iter()
            .map(|component| {
                (
                    component.descriptor.id().clone(),
                    QueryInputDeclaration::from_component(component, factories),
                )
            })
            .collect();

        for record in records {
            if matches!(record.component, OrdinaryComponent::Query { .. }) {
                let factory = self.query_factory().ok_or(QueryFactoryNotRegistered)?;
                let descriptor = factory
                    .query_authoring()
                    .ok_or(QueryFactoryNotRegistered)?
                    .query_descriptor(record.id.clone())?;
                anyhow::ensure!(
                    descriptor.id() == &record.id,
                    "query factory substituted its assigned UUID"
                );
                projected.insert(
                    record.id.clone(),
                    QueryInputDeclaration {
                        descriptor,
                        primary_ports: factory.primary_ports(),
                    },
                );
            }
        }

        for record in records
            .iter()
            .filter(|record| !matches!(record.component, OrdinaryComponent::Query { .. }))
        {
            let update = updates
                .iter()
                .find(|update| update.component_id == record.id);
            let retaining =
                update.is_some_and(|update| update.plan.mode != ComponentUpdateMode::Recreate);
            let local = if let OrdinaryComponent::ProcessLocal { binding, .. } = &record.component {
                let resource = match pending.get(binding) {
                    Some(resource) => resource.clone(),
                    None => publication.resource(binding).map_err(|error| {
                        anyhow::anyhow!(
                            "process-local component {} is unavailable for reconstruction: {error}",
                            record.id
                        )
                    })?,
                };
                let instance = resource.get::<RuntimeInstance>()?;
                let local = instance.record()?;
                anyhow::ensure!(
                    local.node == record.id && local.resource == *binding,
                    "process-local binding belongs to another component"
                );
                anyhow::ensure!(
                    !instance.cleanup_started()
                        || Self::retains_local_binding(&publication, &record.id, binding),
                    "process-local binding was already removed"
                );
                Some(local)
            } else {
                None
            };
            let failed = publication
                .observed
                .components
                .get(&record.id)
                .is_some_and(|state| {
                    state.realization == RealizationState::CreationFailed && state.failure.is_some()
                })
                || publication
                    .desired
                    .specifications
                    .get(&record.id)
                    .into_iter()
                    .flat_map(|spec| spec.dependencies.values().flatten())
                    .any(|id| {
                        publication
                            .observed
                            .resources
                            .get(id)
                            .is_some_and(|state| state.failure.is_some())
                    });
            let retained = current
                .get(&record.id)
                .filter(|current| !current.owner.cleanup_started())
                .filter(|_| !failed || retaining)
                .filter(|_| {
                    retaining
                        || publication
                            .desired
                            .specifications
                            .get(&record.id)
                            .and_then(|spec| spec.configuration.get("definition"))
                            .and_then(|value| match value {
                                ConfigurationValue::Literal(value) => {
                                    serde_json::from_value::<OrdinaryComponentRecord>(value.clone())
                                        .ok()
                                }
                                _ => None,
                            })
                            .is_some_and(|old| record.same_construction(&old))
                });
            if let Some(local) = local {
                if let Value::Source(source) = &local.value {
                    sources.insert(record.id.clone(), source.borrowed.clone());
                }
                if let Value::Reaction(reaction) = &local.value {
                    reaction.bind_committed(&record.references()?)?;
                }
                let retained = current
                    .get(&record.id)
                    .is_some_and(|existing| existing.resource == local.resource);
                let declaration = if retained {
                    let inputs = record
                        .references()?
                        .iter()
                        .filter_map(|input| input.as_id().cloned())
                        .collect();
                    retain_members(
                        previous,
                        &publication,
                        &BTreeSet::from([record.id.clone()]),
                        &inputs,
                    )?
                } else {
                    self.record_batch(record, &local)?
                };
                merge(&mut batch, declaration)?;
            } else if let Some(retained) = retained {
                if let Value::Source(source) = &retained.value {
                    sources.insert(record.id.clone(), source.borrowed.clone());
                }
                if let Value::Reaction(reaction) = &retained.value {
                    reaction.bind_committed(&record.references()?)?;
                }
                let subset = retain_members(
                    previous,
                    &publication,
                    &BTreeSet::from([record.id.clone()]),
                    &BTreeSet::new(),
                )?;
                merge(&mut batch, subset)?;
            } else {
                let definition = if retaining {
                    match &update.expect("retained update").current {
                        crate::management::ComponentUpdateDefinition::Ordinary(current) => {
                            current.clone()
                        }
                        _ => anyhow::bail!("ordinary update has a native creation description"),
                    }
                } else {
                    record.clone()
                };
                let directive = update
                    .filter(|update| update.plan.mode == ComponentUpdateMode::Recreate)
                    .map(crate::management::AcceptedComponentUpdate::directive);
                let host = resolver.clone();
                let scope = self.config.id.clone();
                let value = match &record.component {
                    OrdinaryComponent::Source { .. } => {
                        let source = SourceInstance::deferred(
                            source_creation::DeferredSource::new(
                                record.id.clone(),
                                record.auto_start,
                                Box::new(move |_| {
                                    Box::pin(async move {
                                        host.create_source(&scope, &definition, directive.as_ref())
                                            .await
                                    })
                                }),
                            ),
                            self.services.clone(),
                            self.changed.clone(),
                            self.control()?,
                            Arc::downgrade(self),
                        );
                        sources.insert(record.id.clone(), source.borrowed.clone());
                        Value::Source(source)
                    }
                    OrdinaryComponent::Reaction { inputs, .. } => {
                        Value::Reaction(ReactionInstance::deferred(
                            reaction_creation::DeferredReaction::new_committed(
                                record.id.clone(),
                                inputs.clone(),
                                record.auto_start,
                                ReactionConstructor::Async(Box::new(move |_, inputs| {
                                    Box::pin(async move {
                                        host.create_reaction(
                                            &scope,
                                            &definition,
                                            &inputs,
                                            directive.as_ref(),
                                        )
                                        .await
                                    })
                                })),
                            ),
                            self,
                        ))
                    }
                    OrdinaryComponent::Query { .. } => {
                        unreachable!("queries are compiled separately")
                    }
                    OrdinaryComponent::ProcessLocal { .. } => {
                        unreachable!("process-local bindings are handled before reconstruction")
                    }
                };
                if let Value::Reaction(reaction) = &value {
                    reaction.bind_committed(&record.references()?)?;
                }
                merge(&mut batch, self.managed_record(record, value)?)?;
            }
            let node = batch
                .definition
                .components
                .iter_mut()
                .find(|node| node.descriptor.id() == &record.id)
                .ok_or_else(|| anyhow::anyhow!("managed ordinary declaration is missing"))?;
            node.name = record.name.clone();
            node.lifecycle.auto_start = record.auto_start;
            if update.is_some() {
                if let ComponentConstruction::Factory(specification) = &mut node.construction {
                    let current = specification
                        .configuration
                        .get("definition")
                        .and_then(|value| {
                            let ConfigurationValue::Literal(value) = value else {
                                return None;
                            };
                            serde_json::from_value::<OrdinaryComponentRecord>(value.clone()).ok()
                        });
                    if !current
                        .as_ref()
                        .is_some_and(|current| record.same_construction(current))
                    {
                        specification.configuration.insert(
                            Arc::from("definition"),
                            ConfigurationValue::Literal(serde_json::to_value(record)?),
                        );
                    }
                }
            }
            projected.insert(
                record.id.clone(),
                QueryInputDeclaration::from_component(node, factories),
            );
            batch.definition.subscriptions.extend(
                record
                    .references()?
                    .into_iter()
                    .filter_map(|reference| reference.as_id().cloned())
                    .map(|from| (from, record.id.clone())),
            );
        }

        for record in records
            .iter()
            .filter(|record| matches!(record.component, OrdinaryComponent::Query { .. }))
        {
            let update = updates
                .iter()
                .find(|update| update.component_id == record.id);
            let retaining =
                update.is_some_and(|update| update.plan.mode != ComponentUpdateMode::Recreate);
            let failed = publication
                .observed
                .components
                .get(&record.id)
                .is_some_and(|state| {
                    state.realization == RealizationState::CreationFailed && state.failure.is_some()
                });
            let reusable = queries
                .get(&record.id)
                .filter(|_| !failed)
                .and_then(|declaration| declaration.managed.as_ref())
                .is_some_and(|old| record.same_construction(old));
            let mut query = if reusable {
                let members = native_query::query_members(&publication, &record.id)
                    .into_keys()
                    .collect();
                let inputs = record
                    .references()?
                    .into_iter()
                    .filter_map(|input| input.as_id().cloned())
                    .collect();
                retain_members(previous, &publication, &members, &inputs)?
            } else {
                let config = record.query_configuration()?;
                let catalog = publication
                    .desired
                    .specifications
                    .get(&record.id)
                    .and_then(|spec| spec.dependencies.get("catalog"))
                    .and_then(|ids| ids.first())
                    .map(|id| publication.resource(id)?.get::<QueryResultsCatalog>())
                    .transpose()?
                    .map(|catalog| (*catalog).clone());
                let context = ManagedQueryContext {
                    record,
                    sources: &sources,
                    components: &projected,
                };
                self.ordinary_query_batch_with_context(&config, catalog, Some(&context))
                    .await?
            };
            if retaining && publication.desired.component_by_id(&record.id).is_some() && !failed {
                let members = native_query::query_members(&publication, &record.id)
                    .into_keys()
                    .collect();
                let inputs = record
                    .references()?
                    .into_iter()
                    .filter_map(|input| input.as_id().cloned())
                    .collect();
                let mut retained = retain_members(previous, &publication, &members, &inputs)?;
                for proposed in &query.definition.components {
                    if proposed.descriptor.id() != &record.id {
                        let current = retained
                            .definition
                            .components
                            .iter()
                            .find(|current| current.descriptor.id() == proposed.descriptor.id())
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "retained query update changes its owned component group"
                                )
                            })?;
                        anyhow::ensure!(
                            current.construction == proposed.construction
                                && current.descriptor == proposed.descriptor
                                && current.streams == proposed.streams,
                            "retained query update changes an owned helper"
                        );
                    }
                }
                let proposed = query
                    .definition
                    .components
                    .iter()
                    .find(|node| node.descriptor.id() == &record.id)
                    .ok_or_else(|| anyhow::anyhow!("query update has no target component"))?
                    .clone();
                let current = retained
                    .definition
                    .components
                    .iter_mut()
                    .find(|node| node.descriptor.id() == &record.id)
                    .ok_or_else(|| anyhow::anyhow!("retained query component is missing"))?;
                *current = proposed;
                query = retained;
            }
            for node in &mut query.definition.components {
                node.lifecycle.auto_start = record.auto_start;
                if node.descriptor.id() == &record.id {
                    node.name = record.name.clone();
                }
            }
            let source_owners: BTreeSet<_> = current
                .values()
                .filter(|record| matches!(record.value, Value::Source(_)))
                .map(|record| record.resource.clone())
                .collect();
            query
                .definition
                .resources
                .retain(|resource| !source_owners.contains(&resource.id));
            query
                .bindings
                .resources
                .retain(|id, _| !source_owners.contains(id));
            for (id, host) in &sources {
                let resource = ComputationPipelineBuilder::source_resource_id(id)?;
                if query
                    .definition
                    .resources
                    .iter()
                    .any(|spec| spec.id == resource)
                {
                    if batch
                        .definition
                        .resources
                        .iter()
                        .any(|spec| spec.id == resource)
                    {
                        query
                            .definition
                            .resources
                            .retain(|spec| spec.id != resource);
                        query.definition.resource_dependencies.remove(&resource);
                        query.bindings.resources.remove(&resource);
                    } else {
                        query.bindings.resources.insert(resource, host.resource());
                    }
                }
            }
            merge(&mut batch, query)?;
        }
        let infrastructure = self.infrastructure_resources(&publication, &BTreeSet::new())?;
        batch
            .definition
            .resources
            .retain(|resource| !infrastructure.contains(&resource.id));
        batch
            .definition
            .resource_dependencies
            .retain(|id, _| !infrastructure.contains(id));
        batch
            .bindings
            .resources
            .retain(|id, _| !infrastructure.contains(id));
        batch
            .bindings
            .resource_constructors
            .retain(|id, _| !infrastructure.contains(id));
        batch.definition.subscriptions.sort();
        batch.definition.subscriptions.dedup();
        batch.bindings.defer_activation = true;
        batch.bindings.deferred_management_validation = true;
        Ok(batch)
    }

    fn managed_record(
        &self,
        definition: &OrdinaryComponentRecord,
        value: Value,
    ) -> anyhow::Result<ComponentBatch> {
        let record = self.new_record(&definition.id, value, HashMap::new(), None)?;
        self.record_batch(definition, &record)
    }

    pub(super) fn new_record(
        &self,
        id: &ComponentId,
        value: Value,
        metadata: HashMap<String, String>,
        bootstrap_recipe: Option<crate::config::BootstrapSnapshot>,
    ) -> anyhow::Result<Record> {
        let token = self.next_token()?;
        // A saved opaque binding must never alias a later runtime's object.
        let resource = ResourceId::try_new(format!("instance/{token}/{}", uuid::Uuid::new_v4()))?;
        let component = value.instance();
        let owner = Arc::new(RuntimeInstance::new(
            component.clone(),
            token,
            &self.config.id,
        ));
        let record = Record {
            token,
            node: id.clone(),
            resource,
            value,
            owner: owner.clone(),
            metadata,
            bootstrap_recipe,
            initial_status: ComponentStatus::Added,
            activation_requested: false,
        };
        owner.bind_record(record.data())?;
        Ok(record)
    }

    pub(super) fn record_batch(
        &self,
        definition: &OrdinaryComponentRecord,
        record: &Record,
    ) -> anyhow::Result<ComponentBatch> {
        let token = record.token;
        let resource = record.resource.clone();
        let component = record.value.instance();
        let owner = record.owner.clone();
        let mut descriptor = ComponentDescriptor::try_new(definition.id.clone(), vec![])?
            .with_semantic_kind(definition.kind());
        let mut validation_error = None;
        if let (Some(id), Some(version)) = (
            record.metadata.get("pluginId"),
            record.metadata.get("pluginVersion"),
        ) {
            match descriptor.clone().with_plugin_identity(PluginIdentity {
                id: id.as_str().into(),
                version: version.as_str().into(),
            }) {
                Ok(assigned) => descriptor = assigned,
                Err(error) => {
                    validation_error = Some(Arc::new(GraphError::Validation {
                        component: definition.id.clone(),
                        source: error.into(),
                    }))
                }
            }
        }
        let mut specification = ComponentSpecification {
            descriptor,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    "id".into(),
                    ConfigurationValue::Literal(definition.id.to_string().into()),
                ),
                (
                    "kind".into(),
                    ConfigurationValue::Literal(component.kind().into()),
                ),
                (
                    "definition".into(),
                    ConfigurationValue::Literal(serde_json::to_value(definition)?),
                ),
                (
                    "record_token".into(),
                    ConfigurationValue::Literal(token.into()),
                ),
            ]),
            dependencies: [(Arc::from("instance"), vec![resource.clone()])]
                .into_iter()
                .chain(self.services.dependencies())
                .collect(),
        };
        let mut topology = crate::management::DesiredInstance::default().topology;
        let mut bindings = TopologyBindings {
            validation_error,
            ..Default::default()
        };
        if let Value::Source(source) = &record.value {
            let host_id = ComputationPipelineBuilder::source_resource_id(&definition.id)?;
            specification
                .dependencies
                .insert("source_host".into(), vec![host_id.clone()]);
            topology.resources.push(ResourceSpecification {
                id: host_id.clone(),
                role: ResourceRole::LegacySource,
                ownership: ResourceOwnership::Borrowed,
                binding: definition.id.as_str().into(),
            });
            bindings
                .resources
                .insert(host_id, source.borrowed.resource());
        }
        topology.components.push(DesiredComponent {
            name: definition.name.clone(),
            descriptor: specification.descriptor.clone(),
            role: specification.role,
            completion: None,
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy {
                auto_start: definition.auto_start,
            },
            input_merge: InputMergePolicy::default(),
            construction: ComponentConstruction::Factory(specification),
        });
        topology.resources.push(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::Component,
            ownership: ResourceOwnership::Graph,
            binding: format!("{}:{}", component.kind(), definition.id).into(),
        });
        bindings.factories.register(self.factory.clone())?;
        bindings.resources.insert(
            resource,
            ResourceHandle::new(ResourceRole::Component, owner.clone()).with_cleanup(owner),
        );
        Ok(ComponentBatch {
            definition: topology,
            bindings,
        })
    }
}

fn retain_members(
    previous: &DesiredTopology,
    publication: &GraphRegistrySnapshot,
    members: &BTreeSet<ComponentId>,
    inputs: &BTreeSet<ComponentId>,
) -> anyhow::Result<ComponentBatch> {
    let mut definition = previous.clone();
    definition
        .components
        .retain(|component| members.contains(component.descriptor.id()));
    definition.relationships.retain(|edge| {
        members.contains(&edge.definition.to.component)
            && (members.contains(&edge.definition.from.component)
                || inputs.contains(&edge.definition.from.component))
    });
    definition.boundary_relationships.retain(|edge| {
        members.contains(&edge.definition.to.component)
            && inputs.contains(&edge.definition.from.component)
    });
    definition
        .subscriptions
        .retain(|(from, to)| members.contains(to) && inputs.contains(from));
    definition.control_connections.clear();
    definition
        .readiness_required
        .retain(|id| members.contains(id));
    definition
        .component_plugins
        .retain(|id, _| members.contains(id));
    definition
        .component_resources
        .retain(|id, _| members.contains(id));
    definition
        .recovery_requirements
        .retain(|requirement| members.contains(&requirement.consumer));
    let mut resources: BTreeSet<_> = definition
        .components
        .iter()
        .filter_map(|component| match &component.construction {
            ComponentConstruction::Factory(spec) => Some(spec),
            _ => None,
        })
        .flat_map(|spec| spec.dependencies.values().flatten().cloned())
        .collect();
    for edge in definition
        .relationships
        .iter()
        .chain(&definition.boundary_relationships)
    {
        resources.extend(edge.pipe.resource_dependencies().into_keys());
    }
    resources.extend(definition.component_resources.values().flatten().cloned());
    previous.include_resource_dependencies(&mut resources);
    definition
        .resources
        .retain(|resource| resources.contains(&resource.id));
    definition
        .resource_configurations
        .retain(|id, _| resources.contains(id));
    definition
        .resource_dependencies
        .retain(|id, _| resources.contains(id));
    let mut bindings = TopologyBindings::default();
    for resource in &definition.resources {
        if publication
            .observed
            .resources
            .get(&resource.id)
            .is_some_and(|state| {
                state.realization == ResourceRealization::Created && state.failure.is_none()
            })
        {
            bindings
                .resources
                .insert(resource.id.clone(), publication.resource(&resource.id)?);
        }
    }
    Ok(ComponentBatch {
        definition,
        bindings,
    })
}

pub(crate) fn merge(target: &mut ComponentBatch, addition: ComponentBatch) -> anyhow::Result<()> {
    for resource in addition.definition.resources {
        if let Some(existing) = target
            .definition
            .resources
            .iter()
            .find(|current| current.id == resource.id)
        {
            anyhow::ensure!(
                existing == &resource,
                "conflicting ordinary resource {}",
                resource.id
            );
        } else {
            target.definition.resources.push(resource);
        }
    }
    target
        .definition
        .components
        .extend(addition.definition.components);
    target
        .definition
        .relationships
        .extend(addition.definition.relationships);
    target
        .definition
        .boundary_relationships
        .extend(addition.definition.boundary_relationships);
    target
        .definition
        .subscriptions
        .extend(addition.definition.subscriptions);
    target
        .definition
        .control_connections
        .extend(addition.definition.control_connections);
    merge_map(
        &mut target.definition.resource_configurations,
        addition.definition.resource_configurations,
    )?;
    merge_map(
        &mut target.definition.resource_dependencies,
        addition.definition.resource_dependencies,
    )?;
    merge_map(
        &mut target.definition.component_resources,
        addition.definition.component_resources,
    )?;
    merge_map(
        &mut target.definition.component_plugins,
        addition.definition.component_plugins,
    )?;
    target
        .definition
        .readiness_required
        .extend(addition.definition.readiness_required);
    target.definition.requirements = PipeRequirements::new(
        target
            .definition
            .requirements
            .required()
            .iter()
            .chain(addition.definition.requirements.required())
            .copied(),
    );
    for requirement in addition.definition.recovery_requirements {
        if !target
            .definition
            .recovery_requirements
            .contains(&requirement)
        {
            target.definition.recovery_requirements.push(requirement);
        }
    }
    target.bindings.merge(addition.bindings)?;
    Ok(())
}

fn merge_map<K: Ord + std::fmt::Display, V: PartialEq>(
    target: &mut BTreeMap<K, V>,
    addition: BTreeMap<K, V>,
) -> anyhow::Result<()> {
    for (key, value) in addition {
        if let Some(existing) = target.get(&key) {
            anyhow::ensure!(
                existing == &value,
                "conflicting ordinary definition for {key}"
            );
        } else {
            target.insert(key, value);
        }
    }
    Ok(())
}
