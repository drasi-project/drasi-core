// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::OrdinaryQueryDeclaration;

fn declaration_id(query: &str) -> anyhow::Result<ResourceId> {
    let encoded: String = query.bytes().map(|byte| format!("{byte:02x}")).collect();
    Ok(ResourceId::try_new(format!("query-declaration/{encoded}"))?)
}

impl Runtime {
    pub(crate) fn component_handle(&self, id: &ComponentId) -> anyhow::Result<ComponentHandle> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let handle = control.component_handle(id)?;
        if self.ordinary_queries_at(&publication)?.contains_key(id) {
            return Ok(handle
                .with_lifecycle_members(super::native_query::query_members(&publication, id))?);
        }
        Ok(handle)
    }

    pub(super) async fn start_query_handle(
        self: &Arc<Self>,
        handle: &ComponentHandle,
    ) -> anyhow::Result<()> {
        handle.observed()?;
        let control = self.control()?;
        let publication = control.registry_snapshot();
        anyhow::ensure!(
            publication
                .observed
                .components
                .get(handle.id())
                .is_some_and(|observed| observed.generation == handle.generation()),
            GraphError::StaleGeneration
        );
        let mut selected = handle.clone();
        if self
            .ordinary_queries_at(&publication)?
            .get(handle.id())
            .is_some_and(|declaration| declaration.failure.is_some())
        {
            let config = self.native_query_configuration_at(&publication, handle.id().as_str())?;
            let plan = self
                .ordinary_query_update_plan(&publication, handle.id().as_str(), config, false)
                .await?;
            for resource in plan
                .bindings
                .resources
                .values()
                .filter(|resource| resource.is::<OrdinaryQueryDeclaration>())
            {
                if let Some(cause) = &resource.get::<OrdinaryQueryDeclaration>()?.failure {
                    return Err(GraphError::Reported {
                        cause: cause.clone(),
                    }
                    .into());
                }
            }
            let report = control
                .mutate_members(plan.members, plan.changes, plan.bindings, BTreeSet::new())
                .await?;
            if !report.committed {
                return Err(LifecycleReportError::Reconciliation(report).into());
            }
            selected = control.component_handle(handle.id())?;
        }
        let current = control.registry_snapshot();
        let members = super::native_query::query_members(&current, selected.id());
        if members
            .keys()
            .any(|id| current.observed.components[id].failure.is_some())
        {
            let stopped = control.stop_members(members).await?;
            if stopped.summary != OperationSummary::Completed {
                return Err(LifecycleReportError::Stop(stopped).into());
            }
        }
        let result =
            super::native_query::start_query(&control, selected.id(), Some(selected.generation()))
                .await;
        result.map_err(|error| {
            let publication = control.registry_snapshot();
            let mut failures = vec![error];
            if publication
                .observed
                .components
                .get(selected.id())
                .is_some_and(|observed| observed.generation == selected.generation())
            {
                for member in super::native_query::query_members(&publication, selected.id()).keys()
                {
                    if let Some(failure) = publication
                        .observed
                        .components
                        .get(member)
                        .and_then(|observed| observed.failure.as_ref())
                    {
                        failures.push(anyhow::Error::new(GraphError::Reported {
                            cause: failure.cause.clone(),
                        }));
                    }
                }
            }
            let recovery = publication
                .desired
                .specifications
                .get(selected.id())
                .and_then(|spec| spec.configuration.get("recovery"))
                .and_then(|value| match value {
                    ConfigurationValue::Literal(value) => value.as_str(),
                    _ => None,
                })
                .unwrap_or("strict");
            let recovery = if recovery == "auto_reset" {
                "AutoReset"
            } else {
                "Strict"
            };
            anyhow::Error::new(crate::error::OperationFailures::new(
                format!(
                    "Query '{}' startup under {recovery} recovery failed",
                    selected.id()
                ),
                failures,
            ))
        })
    }

    pub(super) async fn ordinary_query_update_plan(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &str,
        config: QueryConfig,
        reset_configuration: bool,
    ) -> anyhow::Result<super::native_query::NativeQueryUpdate> {
        anyhow::ensure!(
            config.id == id,
            "replacement query ID does not match '{id}'"
        );
        let query = ComponentId::try_new(id)?;
        let mut members = super::native_query::query_members(publication, &query);
        let outlet_implementation = QueryResultsOutletFactory::default()
            .descriptor()
            .implementation
            .clone();
        let outlets: BTreeSet<_> = members
            .keys()
            .filter(|id| {
                publication
                    .desired
                    .specifications
                    .get(*id)
                    .is_some_and(|spec| spec.implementation == outlet_implementation)
            })
            .cloned()
            .collect();
        let catalog_id = publication
            .desired
            .specifications
            .get(&query)
            .and_then(|spec| spec.dependencies.get("catalog"))
            .and_then(|resources| resources.first())
            .cloned()
            .or_else(|| {
                outlets.iter().find_map(|id| {
                    publication
                        .desired
                        .specifications
                        .get(id)
                        .and_then(|spec| spec.dependencies.get("catalog"))
                        .and_then(|resources| resources.first())
                        .cloned()
                })
            });
        let catalog = catalog_id
            .as_ref()
            .map(|id| {
                publication
                    .resource(id)?
                    .get::<QueryResultsCatalog>()
                    .map(|catalog| catalog.as_ref().clone())
            })
            .transpose()?;
        members.retain(|id, _| !outlets.contains(id));
        let selected: BTreeSet<_> = members.keys().cloned().collect();
        anyhow::ensure!(
            publication
                .desired
                .edges
                .iter()
                .filter(|edge| edge.definition.to.component == query)
                .all(|edge| selected.contains(&edge.definition.from.component)),
            "query '{id}' has custom input connections; use computation topology operations"
        );
        let mut batch = self.ordinary_query_batch(&config, catalog).await?;
        if reset_configuration {
            for component in &mut batch.definition.components {
                if component.descriptor.id() == &query {
                    if let ComponentConstruction::Factory(specification) =
                        &mut component.construction
                    {
                        specification.configuration.insert(
                            "reset_configuration".into(),
                            ConfigurationValue::Literal(true.into()),
                        );
                    }
                }
            }
        }
        if !outlets.is_empty() {
            let generated_outlets: BTreeSet<_> = batch.definition.components.iter()
                .filter(|node| matches!(&node.construction, ComponentConstruction::Factory(spec) if spec.implementation == outlet_implementation))
                .map(|node| node.descriptor.id().clone()).collect();
            batch
                .definition
                .components
                .retain(|node| !generated_outlets.contains(node.descriptor.id()));
            batch
                .definition
                .relationships
                .retain(|edge| !generated_outlets.contains(&edge.definition.to.component));
        }
        for resource in &batch.definition.resources {
            if (catalog_id.as_ref() == Some(&resource.id)
                || resource.role == ResourceRole::Middleware)
                && publication.desired.resources.get(&resource.id) == Some(resource)
            {
                batch
                    .bindings
                    .resources
                    .insert(resource.id.clone(), publication.resource(&resource.id)?);
            }
        }
        let candidates = publication
            .desired
            .select(GraphSelection::Exact(selected.iter().cloned().collect()))?
            .resources;
        let mut target = publication.desired.select(GraphSelection::All)?;
        let mut changes = Vec::new();
        target.relationships.retain(|edge| {
            let touches = selected.contains(&edge.definition.from.component)
                || selected.contains(&edge.definition.to.component);
            let external_output = edge.definition.from.component == query
                && !selected.contains(&edge.definition.to.component);
            if touches && !external_output {
                changes.push(DesiredMutation::Unbind {
                    edge: edge.definition.clone(),
                    policy: RemovalPolicy::Reject,
                });
                false
            } else {
                true
            }
        });
        let replacements: BTreeSet<_> = batch
            .definition
            .components
            .iter()
            .map(|component| component.descriptor.id().clone())
            .collect();
        let removed: Vec<_> = selected.difference(&replacements).cloned().collect();
        if !removed.is_empty() {
            changes.push(DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(removed),
                policy: RemovalPolicy::Reject,
            });
        }
        target
            .components
            .retain(|component| !selected.contains(component.descriptor.id()));
        target
            .component_resources
            .retain(|component, _| !selected.contains(component));
        target
            .components
            .extend(batch.definition.components.iter().cloned());
        changes.extend(
            batch
                .definition
                .components
                .iter()
                .cloned()
                .map(DesiredMutation::ReplaceComponent),
        );
        target
            .relationships
            .extend(batch.definition.relationships.iter().cloned());
        changes.extend(
            batch
                .definition
                .relationships
                .iter()
                .cloned()
                .map(DesiredMutation::Bind),
        );
        for consumer in &replacements {
            changes.push(DesiredMutation::SetSubscriptions {
                consumer: consumer.clone(),
                producers: batch
                    .definition
                    .subscriptions
                    .iter()
                    .filter(|(_, target)| target == consumer)
                    .map(|(source, _)| source.clone())
                    .collect(),
            });
        }
        target
            .subscriptions
            .retain(|(_, consumer)| !selected.contains(consumer));
        target
            .subscriptions
            .extend(batch.definition.subscriptions.iter().cloned());
        changes.extend(super::native_query::query_resource_changes(
            publication,
            &mut target,
            &mut batch,
        )?);
        let infrastructure = self.infrastructure_resources(publication, &BTreeSet::new())?;
        changes.extend(super::native_query::remove_unused_resources(
            &mut target,
            candidates,
            &infrastructure,
        )?);
        for (source, _) in &batch.definition.subscriptions {
            if let Some(observed) = publication.observed.components.get(source) {
                members.insert(source.clone(), observed.generation);
            }
        }
        batch.bindings.defer_activation = true;
        Ok(super::native_query::NativeQueryUpdate {
            members,
            changes,
            bindings: batch.bindings,
            running: is_active(&publication.observed.components[&query]),
        })
    }

    pub(crate) fn ordinary_queries_at(
        &self,
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<BTreeMap<ComponentId, Arc<OrdinaryQueryDeclaration>>> {
        let implementation = ContinuousQueryFactory::default()
            .descriptor()
            .implementation
            .clone();
        let mut result = BTreeMap::new();
        for (id, specification) in &publication.desired.specifications {
            if specification.implementation != implementation {
                continue;
            }
            let Some(resources) = specification.dependencies.get("declaration") else {
                continue;
            };
            anyhow::ensure!(
                resources.len() == 1,
                "query '{id}' has no unique declaration"
            );
            let declaration = publication
                .resource(&resources[0])?
                .get::<OrdinaryQueryDeclaration>()?;
            anyhow::ensure!(&declaration.id == id, "query declaration identity mismatch");
            result.insert(id.clone(), declaration);
        }
        Ok(result)
    }

    pub(super) async fn ordinary_query_batch(
        &self,
        config: &QueryConfig,
        catalog: Option<QueryResultsCatalog>,
    ) -> anyhow::Result<ComponentBatch> {
        let publication = self.control()?.registry_snapshot();
        let id = ComponentId::try_new(config.id.as_str())?;
        let prepared = async {
            if let Some(crate::indexes::StorageBackendRef::Inline(specification)) =
                &config.storage_backend
            {
                specification.validate().map_err(|error| {
                    anyhow::anyhow!(
                        "Query '{}' has invalid inline storage backend configuration: {error}",
                        config.id
                    )
                })?;
            }
            self.config
                .index_factory
                .computation_backend(config.storage_backend.as_ref())?;
            let mut builder = ComputationPipelineBuilder::new(
                &publication.desired.id,
                self.services.clone(),
                self.middleware.clone(),
                self.config.index_factory.clone(),
                self.config.default_recovery_policy.unwrap_or_default(),
                self.config.global_priority_queue_capacity.unwrap_or(10_000),
                self.config.global_dispatch_buffer_capacity.unwrap_or(1_000),
            )?;
            if let Some(catalog) = catalog {
                builder = builder.with_catalog(catalog);
            }
            for source in &config.sources {
                let host =
                    match self.source_host(&source.source_id).await {
                        Ok(host) => Some(host),
                        Err(error)
                            if error
                                .downcast_ref::<crate::error::ComponentNotFoundError>()
                                .is_some()
                                && !publication.desired.nodes.iter().any(|node| {
                                    node.descriptor.id().as_str() == source.source_id
                                }) =>
                        {
                            None
                        }
                        Err(error) => {
                            return Err(error
                                .context(format!("Source '{}' is unavailable", source.source_id)))
                        }
                    };
                builder = builder.source_binding(
                    &source.source_id,
                    host,
                    SourceSubscriptionOptions {
                        borrowed_recovery: true,
                        allow_broadcast_loss: true,
                        ..Default::default()
                    },
                )?;
            }
            let batch = builder.query(config.clone()).build()?;
            Ok::<_, anyhow::Error>(batch)
        }
        .await;
        let (mut batch, failure) = match prepared {
            Ok(batch) => (batch, None),
            Err(error) => {
                let implementation = ContinuousQueryFactory::default()
                    .descriptor()
                    .implementation
                    .clone();
                let factory = FactoryRegistry::standard()
                    .get(&implementation)
                    .expect("standard query factory")
                    .clone();
                let specification = factory.specification_from_config(
                    id.clone(),
                    serde_json::json!({
                        "query": config.query,
                        "language": match config.query_language {
                            crate::config::QueryLanguage::Cypher => "cypher",
                            crate::config::QueryLanguage::GQL => "gql",
                        },
                        "stream": format!("{}/out", config.id),
                        "outbox_capacity": config.outbox_capacity.clamp(1, 1_000_000),
                        "execution": QueryExecutionSettings::from_legacy_config(config),
                        "query_config": config,
                        "defer_build": true
                    }),
                )?;
                let mut definition = publication
                    .desired
                    .select(GraphSelection::Exact(Vec::new()))?;
                definition.components.push(DesiredComponent {
                    descriptor: specification.descriptor.clone(),
                    role: ComponentRole::Query,
                    completion: None,
                    streams: BTreeMap::from([(
                        PortId::try_new("out")?,
                        StreamId::try_new(format!("{}/out", config.id))?,
                    )]),
                    lifecycle: LifecyclePolicy {
                        auto_start: config.auto_start,
                    },
                    input_merge: InputMergePolicy::default(),
                    construction: ComponentConstruction::Factory(specification),
                });
                let mut bindings = TopologyBindings::default();
                bindings.factories.register(factory)?;
                (
                    ComponentBatch {
                        definition,
                        bindings,
                    },
                    Some(Arc::new(GraphError::Creation {
                        component: id.clone(),
                        disposition: FailureDisposition::Terminal,
                        source: error,
                    })),
                )
            }
        };
        let declaration = declaration_id(&config.id)?;
        let valid = failure.is_none();
        let backend = self
            .config
            .index_factory
            .configured_provider(config.storage_backend.as_ref());
        let sources = batch
            .bindings
            .resources
            .values()
            .filter(|resource| resource.role() == ResourceRole::LegacySource)
            .map(|resource| resource.get::<SourcePluginHost>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        batch.definition.resources.push(ResourceSpecification {
            id: declaration.clone(),
            role: ResourceRole::Component,
            ownership: ResourceOwnership::Graph,
            binding: Arc::from(format!("query-declaration:{}", config.id)),
        });
        batch.bindings.resources.insert(
            declaration.clone(),
            ResourceHandle::new(
                ResourceRole::Component,
                Arc::new(OrdinaryQueryDeclaration {
                    id: id.clone(),
                    failure,
                    backend: backend.as_ref().map(|(_, provider)| provider.clone()),
                    default_dispatch_capacity: self
                        .config
                        .global_dispatch_buffer_capacity
                        .unwrap_or(1_000),
                    sources,
                }),
            ),
        );
        for component in &mut batch.definition.components {
            component.lifecycle.auto_start = config.auto_start;
            if component.descriptor.id() != &id {
                continue;
            }
            let ComponentConstruction::Factory(specification) = &mut component.construction else {
                anyhow::bail!("ordinary query has no factory declaration");
            };
            specification
                .dependencies
                .insert("declaration".into(), vec![declaration.clone()]);
            if valid {
                specification.dependencies.insert(
                    "source_hosts".into(),
                    config
                        .sources
                        .iter()
                        .map(|source| {
                            ComputationPipelineBuilder::source_resource_id(&source.source_id)
                        })
                        .collect::<GraphResult<Vec<_>>>()?,
                );
            }
            if let Some((name, _)) = &backend {
                specification
                    .dependencies
                    .insert("backend".into(), vec![index_resource_id(name)?]);
            }
        }
        if valid {
            self.bind_batch_source_dependencies(&mut batch).await?;
        }
        batch.bindings.deferred_management_validation = true;
        Ok(batch)
    }

    pub(super) async fn install_ordinary_query(
        &self,
        mut batch: ComponentBatch,
        id: ComponentId,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let mut shared = BTreeSet::new();
        for resource in &batch.definition.resources {
            if let Some(existing) = publication.desired.resources.get(&resource.id) {
                let matches = if let Some(supplied) = batch.bindings.resources.get(&resource.id) {
                    publication
                        .resource(&resource.id)?
                        .same_shared_instance(supplied)
                } else {
                    publication
                        .observed
                        .resources
                        .get(&resource.id)
                        .is_some_and(|state| {
                            state.realization == ResourceRealization::Pending
                                && state.failure.is_none()
                        })
                };
                anyhow::ensure!(
                    existing == resource && matches,
                    "query resource '{}' conflicts with an existing binding",
                    resource.id
                );
                shared.insert(resource.id.clone());
            }
        }
        batch
            .definition
            .resources
            .retain(|resource| !shared.contains(&resource.id));
        batch
            .bindings
            .resources
            .retain(|resource, _| !shared.contains(resource));
        batch.bindings.defer_activation |= !start;
        let owner = self
            .instance_owner
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("instance ownership is unavailable"))?;
        let pending = owner.retain(batch, Some(control.clone()));
        let result = async {
            owner.get().await?;
            control.require_configuration_write()?;
            let report = control.add_components(pending.take().await?).await?;
            if !report.committed {
                return Err(LifecycleReportError::Reconciliation(report).into());
            }
            self.component_handle(&id)
        }
        .await;
        match result {
            Ok(handle) => {
                if let Err(error) = pending.cleanup().await {
                    return Err(super::super::instance::components_cleanup_error(
                        pending, error, None,
                    )
                    .into());
                }
                Ok(handle)
            }
            Err(error) => Err(
                super::super::instance::reject_components(pending, error.into())
                    .await
                    .into(),
            ),
        }
    }
}
