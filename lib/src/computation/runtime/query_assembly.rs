// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::OrdinaryQueryDeclaration;

pub(super) fn bind_inputs(
    config: &mut QueryConfig,
    publication: &GraphRegistrySnapshot,
) -> Vec<DependencyWait> {
    let mut waiting = Vec::new();
    for input in &mut config.inputs {
        if let Some(name) = input.component.as_name() {
            if let Some(node) = publication.desired.component_by_name(name) {
                input.component =
                    crate::config::ComponentReference::by_id(node.descriptor.id().clone());
            } else {
                waiting.push(DependencyWait::ComponentName { name: name.clone() });
            }
        }
    }
    waiting
}

fn declaration_id(query: &ComponentId) -> anyhow::Result<ResourceId> {
    let encoded: String = query
        .as_str()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(ResourceId::try_new(format!("query-declaration/{encoded}"))?)
}

impl Runtime {
    pub(crate) async fn refresh_pending_query_inputs(&self) -> anyhow::Result<()> {
        let _mutation = self.mutations.lock().await;
        self.refresh_pending_query_inputs_locked().await
    }

    pub(super) async fn refresh_pending_query_inputs_locked(&self) -> anyhow::Result<()> {
        let control = self.control()?;
        let mut attempted = BTreeSet::new();
        loop {
            let publication = control.registry_snapshot();
            let mut candidate = None;
            for (query, declaration) in self.ordinary_queries_at(&publication)? {
                if attempted.contains(&query)
                    || declaration.managed.is_some()
                    || declaration.failure.is_some()
                    || publication
                        .observed
                        .components
                        .get(&query)
                        .is_some_and(|observed| {
                            super::is_active(observed) || observed.failure.is_some()
                        })
                {
                    continue;
                }
                let mut config = self.native_query_configuration_at(&publication, &query)?;
                let original_inputs = config.inputs.clone();
                bind_inputs(&mut config, &publication);
                if original_inputs != config.inputs {
                    candidate = Some((query, config, Vec::new(), true));
                    break;
                }
                let waiting_for_native = config.inputs.iter().any(|input| {
                    let available = publication.desired.nodes.iter().any(|node| {
                        input.component.as_id() == Some(node.descriptor.id())
                            && node
                                .descriptor
                                .ports()
                                .iter()
                                .any(|port| port.direction() == PortDirection::Output)
                    });
                    let connected = publication.desired.edges.iter().any(|edge| {
                        input.component.as_id() == Some(&edge.definition.from.component)
                            && edge.definition.to.component == query
                    }) || publication.desired.unbound_relationships.iter().any(
                        |edge| {
                            input.component.as_id() == Some(&edge.definition.from.component)
                                && edge.definition.to.component == query
                        },
                    );
                    available && !connected
                });
                if waiting_for_native {
                    let producers: Vec<_> = config
                        .inputs
                        .iter()
                        .filter(|input| {
                            publication.desired.nodes.iter().any(|node| {
                                input.component.as_id() == Some(node.descriptor.id())
                                    && node
                                        .descriptor
                                        .ports()
                                        .iter()
                                        .any(|port| port.direction() == PortDirection::Output)
                            })
                        })
                        .filter_map(|input| input.component.as_id().cloned())
                        .collect();
                    candidate = Some((query, config, producers, true));
                    break;
                }
            }
            if candidate.is_none() {
                for (query, specification) in &publication.desired.specifications {
                    if attempted.contains(query)
                        || self
                            .ordinary_queries_at(&publication)?
                            .get(query)
                            .is_some_and(|declaration| declaration.managed.is_some())
                        || !publication.has_query_authoring(specification)
                        || !specification.configuration.contains_key("query_config")
                        || publication
                            .observed
                            .components
                            .get(query)
                            .is_some_and(|observed| {
                                super::is_active(observed) || observed.failure.is_some()
                            })
                    {
                        continue;
                    }
                    let config = self.native_query_configuration_at(&publication, query)?;
                    let mut producers = Vec::new();
                    for input in &config.inputs {
                        if input.component.as_id().is_none() {
                            continue;
                        }
                        let pending = ComputationPipelineBuilder::pending_native_endpoint(
                            query,
                            input.id()?,
                        )?;
                        if publication
                            .desired
                            .unbound_relationships
                            .iter()
                            .any(|edge| {
                                edge.definition.from == pending
                                    && edge.definition.to.component == *query
                            })
                            && publication.desired.nodes.iter().any(|node| {
                                input.component.as_id() == Some(node.descriptor.id())
                                    && node
                                        .descriptor
                                        .ports()
                                        .iter()
                                        .any(|port| port.direction() == PortDirection::Output)
                            })
                        {
                            producers.push(input.id()?.to_owned());
                        }
                    }
                    if !producers.is_empty() {
                        candidate = Some((query.clone(), config, producers, false));
                        break;
                    }
                }
            }
            let Some((query, config, producers, _ordinary)) = candidate else {
                return Ok(());
            };
            let generation = control.component_handle_by_id(&query)?.generation();
            let owner = self.configuration_owner()?;
            let owner = owner
                .get()
                .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
            owner
                .accept_auto(crate::management::AuthoringOperation::QueryUpdate {
                    id: query.clone(),
                    generation,
                    configuration: config,
                })
                .await?;
            let report = owner.reconcile().await?;
            anyhow::ensure!(
                report.applied && report.error.is_none(),
                "resolving query input endpoints is incomplete: {report:?}"
            );
            attempted.insert(query.clone());
            let resolved = control.registry_snapshot();
            let failed_declaration = self
                .ordinary_queries_at(&resolved)?
                .get(&query)
                .is_some_and(|declaration| declaration.failure.is_some());
            if !failed_declaration {
                for producer in producers {
                    anyhow::ensure!(
                        resolved
                            .desired
                            .edges
                            .iter()
                            .map(|edge| &edge.definition)
                            .chain(
                                resolved
                                    .desired
                                    .unbound_relationships
                                    .iter()
                                    .map(|edge| &edge.definition)
                            )
                            .any(|edge| edge.from.component == producer
                                && edge.to.component == query),
                        "resolved query '{query}' omitted its input connection from '{producer}'"
                    );
                }
            }
        }
    }

    pub(crate) fn component_handle(&self, id: &ComponentId) -> anyhow::Result<ComponentHandle> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let handle = control.component_handle_by_id(id)?;
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
            let owner = self.configuration_owner()?;
            let owner = owner
                .get()
                .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
            let status = owner.reconcile().await?;
            anyhow::ensure!(
                status.error.is_none(),
                "query retry failed: {:?}",
                status.error
            );
            if let Some(cause) = self
                .ordinary_queries_at(&control.registry_snapshot())?
                .get(handle.id())
                .and_then(|declaration| declaration.failure.clone())
            {
                return Err(GraphError::Reported { cause }.into());
            }
            selected = control.component_handle_by_id(handle.id())?;
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
        id: &ComponentId,
        config: QueryConfig,
    ) -> anyhow::Result<super::native_query::NativeQueryUpdate> {
        anyhow::ensure!(
            &config.id == id,
            "replacement query ID does not match '{id}'"
        );
        let query = id.clone();
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
        let original = self.native_query_configuration_at(publication, id)?;
        let native_inputs: BTreeMap<_, _> = original
            .inputs
            .iter()
            .enumerate()
            .filter(|(_, input)| {
                publication.desired.nodes.iter().any(|node| {
                    input.component.as_id() == Some(node.descriptor.id())
                        && node
                            .descriptor
                            .ports()
                            .iter()
                            .any(|port| port.direction() == PortDirection::Output)
                })
            })
            .map(|(rank, input)| {
                let explicit = input.port.clone();
                let endpoint = publication.output_endpoint(input.id()?, explicit.as_ref())?;
                Ok((endpoint, rank))
            })
            .collect::<anyhow::Result<_>>()?;
        let scheduled = QueryScheduledSourceFactory::default()
            .descriptor()
            .implementation
            .clone();
        let input_queue = publication.desired.edges.iter().find_map(|edge| {
            if edge.definition.to.component == query
                && publication
                    .desired
                    .specifications
                    .get(&edge.definition.from.component)
                    .is_some_and(|spec| spec.implementation == scheduled)
            {
                if let DesiredPipe::Ranked(pipe) = &edge.pipe {
                    return Some(&pipe.queue);
                }
            }
            None
        });
        let native_policy = RelationshipPolicy {
            activation: if original.enable_bootstrap {
                ActivationCoupling::RequiresRunning
            } else {
                ActivationCoupling::Independent
            },
            ..Default::default()
        };
        anyhow::ensure!(
            publication
                .desired
                .edges
                .iter()
                .filter(|edge| edge.definition.to.component == query)
                .all(|edge| selected.contains(&edge.definition.from.component)
                    || native_inputs.contains_key(&edge.definition.from)
                        && edge.policy == native_policy
                        && matches!(&edge.pipe, DesiredPipe::Ranked(pipe)
                            if Some(&pipe.queue) == input_queue
                                && pipe.source_rank == native_inputs[&edge.definition.from]
                                && !pipe.drop_when_full && pipe.source_id.is_none() && pipe.source_dispatch.is_none())),
            "query '{id}' has custom input connections; use computation topology operations"
        );
        let mut batch = self.ordinary_query_batch(&config, catalog).await?;
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
        for relationships in [&mut target.relationships, &mut target.boundary_relationships] {
            relationships.retain(|edge| {
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
        }
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
        target
            .boundary_relationships
            .extend(batch.definition.boundary_relationships.iter().cloned());
        changes.extend(
            batch
                .definition
                .relationships
                .iter()
                .chain(batch.definition.boundary_relationships.iter())
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
            for producer in native_inputs
                .keys()
                .map(|endpoint| &endpoint.component)
                .chain(
                    batch
                        .definition
                        .boundary_relationships
                        .iter()
                        .filter(|edge| edge.definition.to.component == query)
                        .map(|edge| &edge.definition.from.component),
                )
            {
                if let Some(observed) = publication.observed.components.get(producer) {
                    members.insert(producer.clone(), observed.generation);
                }
            }
        }
        batch.bindings.defer_activation = true;
        Ok(super::native_query::NativeQueryUpdate {
            members,
            changes,
            bindings: batch.bindings,
        })
    }

    pub(crate) fn ordinary_queries_at(
        &self,
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<BTreeMap<ComponentId, Arc<OrdinaryQueryDeclaration>>> {
        let mut result = BTreeMap::new();
        for (id, specification) in &publication.desired.specifications {
            if !publication.has_query_authoring(specification) {
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
            if declaration.managed.is_some() {
                let config = self.native_query_configuration_at(publication, id)?;
                let mut declaration = (*declaration).clone();
                declaration.managed = Some(
                    crate::management::OrdinaryComponentDefinition::query(config.into_authoring())?
                        .with_assigned_id(id.clone()),
                );
                result.insert(id.clone(), Arc::new(declaration));
            } else {
                result.insert(id.clone(), declaration);
            }
        }
        Ok(result)
    }

    pub(super) async fn ordinary_query_batch(
        &self,
        config: &QueryConfig,
        catalog: Option<QueryResultsCatalog>,
    ) -> anyhow::Result<ComponentBatch> {
        self.ordinary_query_batch_with_context(config, catalog, None)
            .await
    }

    pub(super) async fn ordinary_query_batch_with_context(
        &self,
        config: &QueryConfig,
        catalog: Option<QueryResultsCatalog>,
        managed: Option<&super::managed::ManagedQueryContext<'_>>,
    ) -> anyhow::Result<ComponentBatch> {
        let factory = self.query_factory().ok_or(QueryFactoryNotRegistered)?;
        let publication = self.control()?.registry_snapshot();
        let mut bound = config.clone();
        let waiting = if managed.is_some() {
            bound
                .inputs
                .iter()
                .filter_map(|input| {
                    input
                        .component
                        .as_name()
                        .map(|name| DependencyWait::ComponentName { name: name.clone() })
                })
                .collect()
        } else {
            bind_inputs(&mut bound, &publication)
        };
        let config = &bound;
        let id = config.id.clone();
        #[cfg(feature = "test-support")]
        let test_control = self
            .ordinary_queries_at(&publication)?
            .get(&id)
            .and_then(|declaration| declaration.test_control.clone())
            .or_else(|| {
                self.test_controls
                    .get()
                    .and_then(|controls| controls.get(&config.name))
                    .cloned()
            });
        let prepared = async {
            anyhow::ensure!(waiting.is_empty(), "query is awaiting named inputs");
            anyhow::ensure!(
                config
                    .inputs
                    .iter()
                    .all(|input| input.component.as_id() != Some(&id)),
                "a query cannot subscribe to itself"
            );
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
            )?
            .query_factory(factory.clone())
            .with_output_catalogue(
                self.config.id.clone(),
                OutputCatalogue::new(self.control()?),
            );
            if let Some(catalog) = catalog {
                builder = builder.with_catalog(catalog);
            }
            #[cfg(feature = "test-support")]
            if let Some(control) = &test_control {
                builder = builder.with_test_control(control.clone());
            }
            for source in &config.inputs {
                if let Some(context) = managed {
                    if let Some(host) = context.sources.get(source.id()?) {
                        anyhow::ensure!(
                            source
                                .port
                                .as_ref()
                                .is_none_or(|port| port.as_str() == "out"),
                            "source '{}' has no output port '{}'",
                            source.component,
                            source.port.as_ref().map(PortId::as_str).unwrap_or_default()
                        );
                        builder = builder.source_binding(
                            source.id()?,
                            Some(host.clone()),
                            SourceSubscriptionOptions {
                                borrowed_recovery: true,
                                allow_broadcast_loss: true,
                                ..Default::default()
                            },
                        )?;
                        continue;
                    }
                    if let Some(component) =
                        context.components.get(source.id()?).filter(|component| {
                            component
                                .descriptor
                                .ports()
                                .iter()
                                .any(|port| port.direction() == PortDirection::Output)
                        })
                    {
                        let primaries = &component.primary_ports;
                        let explicit = source.port.clone();
                        let port = primaries.resolve(
                            component.descriptor.ports(),
                            PortDirection::Output,
                            explicit.as_ref(),
                        )?;
                        builder =
                            builder.native_source(Endpoint::new(source.id()?.clone(), port))?;
                        continue;
                    }
                }
                let declared = publication
                    .desired
                    .nodes
                    .iter()
                    .find(|node| source.component.as_id() == Some(node.descriptor.id()));
                if declared.is_none()
                    && source
                        .port
                        .as_ref()
                        .is_some_and(|port| port.as_str() != "out")
                {
                    builder = builder.native_source(Endpoint::new(
                        source.id()?.clone(),
                        source.port.clone().expect("explicit port"),
                    ))?;
                    continue;
                }
                if publication.desired.nodes.iter().any(|node| {
                    source.component.as_id() == Some(node.descriptor.id())
                        && node
                            .descriptor
                            .ports()
                            .iter()
                            .any(|port| port.direction() == PortDirection::Output)
                }) {
                    let component = source.id()?.clone();
                    let explicit = source.port.clone();
                    let output = publication.output_endpoint(&component, explicit.as_ref())?;
                    builder = builder.native_source(output)?;
                    continue;
                }
                anyhow::ensure!(
                    source
                        .port
                        .as_ref()
                        .is_none_or(|port| port.as_str() == "out"),
                    "source '{}' has no output port '{}'",
                    source.component,
                    source.port.as_ref().map(PortId::as_str).unwrap_or_default()
                );
                let host = match self.source_host(source.id()?).await {
                    Ok(host) => Some(host),
                    Err(error)
                        if error
                            .downcast_ref::<crate::error::ComponentNotFoundError>()
                            .is_some()
                            && !publication.desired.nodes.iter().any(|node| {
                                source.component.as_id() == Some(node.descriptor.id())
                            }) =>
                    {
                        None
                    }
                    Err(error) => {
                        return Err(
                            error.context(format!("Source '{}' is unavailable", source.component))
                        )
                    }
                };
                builder = builder.source_binding(
                    source.id()?,
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
                let factory = factory.clone();
                let mut deferred = serde_json::json!({
                    "query": config.query,
                    "language": match config.query_language {
                        crate::config::QueryLanguage::Cypher => "cypher",
                        crate::config::QueryLanguage::GQL => "gql",
                    },
                    "stream": format!("{}/out", config.id),
                    "outbox_capacity": config.outbox_capacity.clamp(1, 1_000_000),
                    "query_config": config,
                    "defer_build": true
                });
                // Unresolved authoring references are not execution inputs.
                if config
                    .inputs
                    .iter()
                    .all(|input| input.component.as_id().is_some())
                {
                    deferred["execution"] =
                        serde_json::to_value(QueryExecutionSettings::from_configuration(config)?)?;
                }
                let specification = factory.specification_from_config(id.clone(), deferred)?;
                anyhow::ensure!(
                    specification.descriptor.id() == &id,
                    "query factory substituted its assigned UUID"
                );
                let mut definition = publication
                    .desired
                    .select(GraphSelection::Exact(Vec::new()))?;
                definition.components.push(DesiredComponent {
                    name: config.name.clone(),
                    descriptor: specification.descriptor.clone(),
                    role: ComponentRole::Transformer,
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
                    waiting.is_empty().then(|| {
                        Arc::new(GraphError::Creation {
                            component: id.clone(),
                            disposition: FailureDisposition::Terminal,
                            source: error,
                        })
                    }),
                )
            }
        };
        let declaration = declaration_id(&config.id)?;
        let valid = failure.is_none() && waiting.is_empty();
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
                    waiting,
                    backend: backend.as_ref().map(|(_, provider)| provider.clone()),
                    default_dispatch_capacity: self
                        .config
                        .global_dispatch_buffer_capacity
                        .unwrap_or(1_000),
                    sources,
                    managed: managed.map(|context| context.record.clone()),
                    #[cfg(feature = "test-support")]
                    test_control,
                }),
            ),
        );
        let source_hosts: Vec<_> = batch
            .definition
            .resources
            .iter()
            .filter(|resource| resource.role == ResourceRole::LegacySource)
            .map(|resource| resource.id.clone())
            .collect();
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
                specification
                    .dependencies
                    .insert("source_hosts".into(), source_hosts.clone());
            }
            if let Some((name, _)) = &backend {
                specification
                    .dependencies
                    .insert("backend".into(), vec![index_resource_id(name)?]);
            }
        }
        if valid {
            if managed.is_some() {
                for input in &config.inputs {
                    let producer = input.id()?.clone();
                    let reader = ComputationPipelineBuilder::source_component_id(&id, &producer)?;
                    if batch
                        .definition
                        .components
                        .iter()
                        .any(|node| node.descriptor.id() == &reader)
                    {
                        batch
                            .definition
                            .subscriptions
                            .extend([(producer.clone(), id.clone()), (producer, reader)]);
                    }
                }
            } else {
                self.bind_batch_source_dependencies(&mut batch).await?;
            }
        }
        batch.bindings.deferred_management_validation = true;
        Ok(batch)
    }
}
