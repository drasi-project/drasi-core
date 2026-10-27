// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeMap, BTreeSet},
    result::Result,
    sync::Arc,
    time::Duration,
};

use super::Runtime;
use crate::{
    channels::{ChangeReceiver, QueryResult, QuerySubscriptionResponse},
    computation::v1::*,
    config::QueryConfig,
    metrics::QueryOutputMetrics,
    queries::{FetchError, OutboxResponse, Query, SnapshotResponse},
    ComponentStatus,
};
use async_trait::async_trait;

pub(crate) fn query_members(
    publication: &GraphRegistrySnapshot,
    id: &ComponentId,
) -> std::collections::BTreeMap<ComponentId, ComponentGeneration> {
    let reader = SourcePluginAdapterFactory::default()
        .descriptor()
        .implementation
        .clone();
    let timer = QueryScheduledSourceFactory::default()
        .descriptor()
        .implementation
        .clone();
    let outlet = QueryResultsOutletFactory::default()
        .descriptor()
        .implementation
        .clone();
    let owner = std::collections::BTreeSet::from([id.clone()]);
    let mut selected = vec![id.clone()];
    for edge in publication.desired.edges.iter() {
        if &edge.definition.to.component == id {
            let helper = &edge.definition.from.component;
            if publication
                .desired
                .specifications
                .get(helper)
                .is_some_and(|spec| spec.implementation == reader || spec.implementation == timer)
                && publication.desired.data_dependents(helper) == owner
            {
                selected.push(helper.clone());
            }
        } else if &edge.definition.from.component == id {
            let helper = &edge.definition.to.component;
            if publication
                .desired
                .specifications
                .get(helper)
                .is_some_and(|spec| spec.implementation == outlet)
                && publication.desired.data_dependencies(helper) == owner
            {
                selected.push(helper.clone());
            }
        }
    }
    selected.sort();
    selected.dedup();
    selected
        .into_iter()
        .map(|id| {
            let generation = publication.observed.components[&id].generation;
            (id, generation)
        })
        .collect()
}

fn member_needs_stop(observed: &ObservedComponent) -> bool {
    super::needs_stop(observed) || observed.lifecycle == ComponentLifecycle::Stopping
}

pub(super) fn query_needs_stop(publication: &GraphRegistrySnapshot, id: &ComponentId) -> bool {
    query_members(publication, id)
        .keys()
        .any(|member| member_needs_stop(&publication.observed.components[member]))
}

pub(super) fn query_activation_requested(
    publication: &GraphRegistrySnapshot,
    id: &ComponentId,
) -> bool {
    let state = &publication.observed.components[id];
    super::is_active(state)
        || !publication.activation_is_deferred(id)
            && state.failure.is_none()
            && !state.waiting_for.is_empty()
            && (state.lifecycle_requested || publication.desired.lifecycle_policies[id].auto_start)
}

pub(super) async fn start_query(
    control: &GraphControl,
    id: &ComponentId,
    expected: Option<ComponentGeneration>,
) -> anyhow::Result<()> {
    if let Some(report) = request_query_start(control, id, expected).await? {
        if report.summary != OperationSummary::Completed {
            let observed = control.observed();
            let waiting: BTreeSet<_> = report
                .components
                .keys()
                .filter_map(|id| {
                    observed
                        .components
                        .get(id)
                        .and_then(ObservedComponent::waiting_reason)
                })
                .collect();
            anyhow::ensure!(
                waiting.is_empty(),
                "Query '{id}' is not ready: {}",
                waiting.into_iter().collect::<Vec<_>>().join("; ")
            );
            return Err(LifecycleReportError::Start(report).into());
        }
    }
    Ok(())
}

async fn request_query_start(
    control: &GraphControl,
    id: &ComponentId,
    expected: Option<ComponentGeneration>,
) -> anyhow::Result<Option<StartReport>> {
    let publication = control.registry_snapshot();
    let state = publication
        .observed
        .components
        .get(id)
        .ok_or(GraphError::StaleGeneration)?;
    anyhow::ensure!(
        expected.map_or(true, |expected| expected == state.generation),
        GraphError::StaleGeneration
    );
    anyhow::ensure!(
        !matches!(
            state.lifecycle,
            ComponentLifecycle::Starting
                | ComponentLifecycle::Running
                | ComponentLifecycle::Stopping
        ),
        "Query '{id}' cannot start while it is {:?}",
        state.lifecycle
    );
    let members = query_members(&publication, id);
    let declaration = publication
        .desired
        .specifications
        .get(id)
        .filter(|spec| {
            spec.implementation
                == ContinuousQueryFactory::default()
                    .descriptor()
                    .implementation
        })
        .and_then(|spec| spec.dependencies.get("declaration"))
        .and_then(|resources| resources.first())
        .map(|resource| {
            publication
                .resource(resource)?
                .get::<OrdinaryQueryDeclaration>()
        })
        .transpose()?;
    let mut subscriptions = Vec::new();
    let mut waiting_for_subscriptions = false;
    if let Some(declaration) = declaration {
        if let Some(cause) = &declaration.failure {
            return Err(GraphError::Reported {
                cause: cause.clone(),
            }
            .into());
        }
        for member in members.keys() {
            let Some(specification) = publication.desired.specifications.get(member) else {
                continue;
            };
            if let Some(resources) = specification.dependencies.get("subscription") {
                for resource in resources {
                    if !publication
                        .observed
                        .resources
                        .get(resource)
                        .is_some_and(|observed| {
                            observed.realization == ResourceRealization::Created
                        })
                    {
                        waiting_for_subscriptions = true;
                        continue;
                    }
                    let subscription = publication
                        .resource(resource)?
                        .get::<LegacySourceSubscription>()?;
                    subscription.prepare_start();
                    subscriptions.push(subscription);
                }
            }
        }
    }
    let mut activation = Box::pin(control.start_members(members.clone()));
    let report = if subscriptions.is_empty() || waiting_for_subscriptions {
        activation.await?
    } else {
        let subscribed = async {
            for subscription in subscriptions {
                subscription.wait_subscribed().await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::select! {
            biased;
            report = &mut activation => report?,
            result = subscribed => {
                result?;
                // Only the reply waiter is dropped; the graph still owns startup and cleanup.
                return Ok(None);
            }
        }
    };
    Ok(Some(report))
}

pub(super) async fn stop_query(
    control: &GraphControl,
    id: &ComponentId,
    expected: Option<ComponentGeneration>,
) -> anyhow::Result<()> {
    let publication = control.registry_snapshot();
    let state = publication
        .observed
        .components
        .get(id)
        .ok_or(GraphError::StaleGeneration)?;
    anyhow::ensure!(
        expected.map_or(true, |expected| expected == state.generation),
        GraphError::StaleGeneration
    );
    let members = query_members(&publication, id);
    // The evaluator may stop before its subscription and output helpers.
    if state.lifecycle != ComponentLifecycle::Stopped
        || !members
            .keys()
            .any(|member| member_needs_stop(&publication.observed.components[member]))
    {
        super::ensure_stoppable("query", id.as_str(), state)?;
    }
    let report = control.stop_members(members).await?;
    if report.summary != OperationSummary::Completed {
        return Err(LifecycleReportError::Stop(report).into());
    }
    Ok(())
}

fn dependency(specification: &ComponentSpecification, name: &str) -> anyhow::Result<ResourceId> {
    specification
        .dependencies
        .get(name)
        .filter(|resources| resources.len() == 1)
        .and_then(|resources| resources.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("query has no unique '{name}' resource"))
}

pub(super) fn resource_users(
    topology: &DesiredTopology,
    resource: &ResourceId,
) -> BTreeSet<ComponentId> {
    let mut users = topology.component_resource_users(resource);
    let mut affected = BTreeSet::from([resource.clone()]);
    topology.include_resource_dependents(&mut affected);
    for relationship in topology
        .relationships
        .iter()
        .chain(&topology.boundary_relationships)
    {
        if relationship
            .pipe
            .resource_dependencies()
            .keys()
            .any(|id| affected.contains(id))
        {
            users.insert(relationship.definition.from.component.clone());
            users.insert(relationship.definition.to.component.clone());
        }
    }
    users
}

pub(super) fn remove_unused_resources(
    target: &mut DesiredTopology,
    candidates: Vec<ResourceSpecification>,
    preserved: &BTreeSet<ResourceId>,
) -> anyhow::Result<Vec<DesiredMutation>> {
    let mut unused: BTreeSet<_> = candidates
        .into_iter()
        .filter(|resource| {
            !preserved.contains(&resource.id) && resource_users(target, &resource.id).is_empty()
        })
        .map(|resource| resource.id)
        .collect();
    loop {
        let previous = unused.len();
        let required: BTreeSet<_> = target
            .resource_dependencies
            .iter()
            .filter(|(id, _)| !unused.contains(*id))
            .flat_map(|(_, dependencies)| dependencies.keys().cloned())
            .collect();
        unused.retain(|id| !required.contains(id));
        if previous == unused.len() {
            break;
        }
    }
    let changes = target
        .resource_construction_order()?
        .into_iter()
        .rev()
        .filter(|id| unused.contains(id))
        .map(|resource| DesiredMutation::RemoveResource {
            resource,
            policy: RemovalPolicy::Reject,
        })
        .collect();
    target
        .resources
        .retain(|resource| !unused.contains(&resource.id));
    target
        .resource_configurations
        .retain(|id, _| !unused.contains(id));
    target
        .resource_dependencies
        .retain(|id, _| !unused.contains(id));
    Ok(changes)
}

pub(super) fn query_resource_changes(
    publication: &GraphRegistrySnapshot,
    target: &mut DesiredTopology,
    batch: &mut ComponentBatch,
) -> anyhow::Result<Vec<DesiredMutation>> {
    let mut changes = Vec::new();
    for resource in &batch.definition.resources {
        if let Some(existing) = target
            .resources
            .iter_mut()
            .find(|existing| existing.id == resource.id)
        {
            *existing = resource.clone();
        } else {
            target.resources.push(resource.clone());
        }
        let same_declaration = publication.desired.resources.get(&resource.id) == Some(resource);
        let unchanged = if !same_declaration {
            false
        } else if let Some(supplied) = batch.bindings.resources.get(&resource.id) {
            if publication.observed.resources[&resource.id].realization
                == ResourceRealization::Created
            {
                publication
                    .resource(&resource.id)?
                    .same_shared_instance(supplied)
            } else {
                false
            }
        } else {
            !batch
                .bindings
                .resource_constructors
                .contains_key(&resource.id)
                && publication.observed.resources[&resource.id].realization
                    == ResourceRealization::Pending
                && publication.observed.resources[&resource.id]
                    .failure
                    .is_none()
        };
        if unchanged {
            batch.bindings.resources.remove(&resource.id);
        } else {
            changes.push(DesiredMutation::PutResource(resource.clone()));
            if publication.desired.resources.contains_key(&resource.id) {
                changes.push(DesiredMutation::RebindResource(resource.id.clone()));
            }
        }
        if unchanged
            && !batch
                .definition
                .resource_dependencies
                .contains_key(&resource.id)
        {
            continue;
        }
        let dependencies = batch
            .definition
            .resource_dependencies
            .get(&resource.id)
            .cloned()
            .unwrap_or_default();
        if target
            .resource_dependencies
            .get(&resource.id)
            .cloned()
            .unwrap_or_default()
            != dependencies
        {
            changes.push(DesiredMutation::SetResourceDependencies {
                resource: resource.id.clone(),
                dependencies: dependencies.clone(),
            });
            if dependencies.is_empty() {
                target.resource_dependencies.remove(&resource.id);
            } else {
                target
                    .resource_dependencies
                    .insert(resource.id.clone(), dependencies);
            }
        }
    }
    Ok(changes)
}

pub(super) struct NativeQueryUpdate {
    pub(super) members: BTreeMap<ComponentId, ComponentGeneration>,
    pub(super) changes: Vec<DesiredMutation>,
    pub(super) bindings: TopologyBindings,
    pub(super) running: bool,
}

impl NativeQueryUpdate {
    pub(super) fn merge(&mut self, other: Self) -> anyhow::Result<()> {
        for (id, generation) in other.members {
            if let Some(existing) = self.members.insert(id, generation) {
                anyhow::ensure!(existing == generation, GraphError::StaleGeneration);
            }
        }
        self.changes.extend(other.changes);
        self.bindings.merge(other.bindings)?;
        Ok(())
    }
}

impl Runtime {
    pub(super) async fn remove_native_component(
        &self,
        id: &str,
        kind: &str,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let kind = match kind {
            "source" => "source",
            "query" => "query",
            "reaction" => "reaction",
            _ => anyhow::bail!("unsupported component kind {kind}"),
        };
        let observation = self.component_observation_at(&publication, id, kind)?;
        let log_key = self.component_log_key(&publication, id)?;
        let query = ComponentId::try_new(id)?;
        let members = if kind == "query" {
            query_members(&publication, &query)
        } else {
            BTreeMap::from([(query.clone(), observation.generation)])
        };
        let selected: BTreeSet<_> = members.keys().cloned().collect();
        let outlet = QueryResultsOutletFactory::default()
            .descriptor()
            .implementation
            .clone();
        let shared_outlets: BTreeSet<_> = publication
            .desired
            .edges
            .iter()
            .filter(|edge| kind == "query" && edge.definition.from.component == query)
            .filter_map(|edge| {
                let target = &edge.definition.to.component;
                publication
                    .desired
                    .specifications
                    .get(target)
                    .is_some_and(|spec| spec.implementation == outlet)
                    .then(|| target.clone())
            })
            .collect();
        let dependents: BTreeSet<_> = selected
            .iter()
            .flat_map(|id| publication.desired.data_dependents(id))
            .filter(|id| !selected.contains(id) && !shared_outlets.contains(id))
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
        let mut target = publication.desired.select(GraphSelection::All)?;
        let mut changes = Vec::new();
        target.relationships.retain(|edge| {
            if selected.contains(&edge.definition.from.component)
                || selected.contains(&edge.definition.to.component)
            {
                changes.push(DesiredMutation::Unbind {
                    edge: edge.definition.clone(),
                    policy: RemovalPolicy::Reject,
                });
                false
            } else {
                true
            }
        });
        target.boundary_relationships.retain(|edge| {
            !selected.contains(&edge.definition.from.component)
                && !selected.contains(&edge.definition.to.component)
        });
        target
            .components
            .retain(|node| !selected.contains(node.descriptor.id()));
        target
            .component_resources
            .retain(|id, _| !selected.contains(id));
        changes.push(DesiredMutation::RemoveComponents {
            selection: GraphSelection::Exact(selected.iter().cloned().collect()),
            policy: RemovalPolicy::Reject,
        });
        let infrastructure = self.infrastructure_resources(&publication, &BTreeSet::new())?;
        changes.extend(remove_unused_resources(
            &mut target,
            candidates,
            &infrastructure,
        )?);
        let report = control
            .mutate_members(
                members,
                changes,
                TopologyBindings::default(),
                if cleanup {
                    BTreeSet::from([query])
                } else {
                    BTreeSet::new()
                },
            )
            .await?;
        anyhow::ensure!(
            report.committed && report.summary == OperationSummary::Completed,
            "{kind} removal is incomplete: {report:?}"
        );
        self.logs.remove_component_by_key(&log_key).await;
        self.notify();
        Ok(())
    }

    pub(super) async fn update_native_query(
        self: &Arc<Self>,
        id: &str,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        let plan = self.native_query_update_plan(id, config).await?;
        let resume = self.pause_consumers(&[id.to_owned()]).await?;
        let control = self.control()?;
        let report = control
            .mutate_members(plan.members, plan.changes, plan.bindings, BTreeSet::new())
            .await?;
        anyhow::ensure!(
            report.committed && report.summary == OperationSummary::Completed,
            "query replacement is incomplete: {report:?}"
        );
        if plan.running {
            if let Some(started) =
                request_query_start(&control, &ComponentId::try_new(id)?, None).await?
            {
                super::accept_waiting_start(started, &control.observed())?;
            }
        }
        self.resume_records(resume).await
    }

    pub(super) async fn native_query_update_plan(
        &self,
        id: &str,
        config: QueryConfig,
    ) -> anyhow::Result<NativeQueryUpdate> {
        anyhow::ensure!(
            config.id == id,
            "New query ID '{}' does not match existing query ID '{id}'",
            config.id
        );
        let control = self.control()?;
        let publication = control.registry_snapshot();
        self.query_observation_at(&publication, id)?;
        if self
            .ordinary_queries_at(&publication)?
            .contains_key(&ComponentId::try_new(id)?)
        {
            return self
                .ordinary_query_update_plan(&publication, id, config, true)
                .await;
        }
        let query_id = ComponentId::try_new(id)?;
        let original = publication
            .desired
            .specifications
            .get(&query_id)
            .ok_or_else(|| {
                anyhow::anyhow!("query '{id}' has no reconstructible factory configuration")
            })?;
        anyhow::ensure!(
            original.implementation
                == ContinuousQueryFactory::default()
                    .descriptor()
                    .implementation,
            "query '{id}' does not support QueryConfig replacement"
        );
        let original_config: QueryConfig = match original.configuration.get("query_config") {
            Some(ConfigurationValue::Literal(value)) => serde_json::from_value(value.clone())?,
            _ => anyhow::bail!("query '{id}' has explicit graph inputs; update its factory configuration and connections through the computation API"),
        };
        let catalog_id = dependency(original, "catalog")?;
        let middleware_id = dependency(original, "middleware")?;
        let catalog = publication
            .resource(&catalog_id)?
            .get::<QueryResultsCatalog>()?;
        let middleware = publication
            .resource(&middleware_id)?
            .get::<QueryMiddlewareResource>()?;
        let mut builder = ComputationPipelineBuilder::new(
            &publication.desired.id,
            self.services.clone(),
            middleware.0.clone(),
            self.config.index_factory.clone(),
            self.config.default_recovery_policy.unwrap_or_default(),
            self.config.global_priority_queue_capacity.unwrap_or(10_000),
            self.config.global_dispatch_buffer_capacity.unwrap_or(1_000),
        )?
        .with_catalog(catalog.as_ref().clone())
        .with_output_catalogue(
            self.config.id.clone(),
            OutputCatalogue::new(self.control()?),
        );
        let reader = SourcePluginAdapterFactory::default()
            .descriptor()
            .implementation
            .clone();
        let timer = QueryScheduledSourceFactory::default()
            .descriptor()
            .implementation
            .clone();
        let outlet = QueryResultsOutletFactory::default()
            .descriptor()
            .implementation
            .clone();
        let mut sources = BTreeMap::new();
        let mut native_sources = BTreeMap::new();
        let input_queue = publication.desired.edges.iter().find_map(|edge| {
            let specification = publication
                .desired
                .specifications
                .get(&edge.definition.from.component)?;
            match &edge.pipe {
                DesiredPipe::Ranked(pipe)
                    if edge.definition.to.component == query_id
                        && specification.implementation == timer =>
                {
                    Some(pipe.queue.clone())
                }
                _ => None,
            }
        });
        for (edge, pipe, policy) in publication
            .desired
            .edges
            .iter()
            .map(|edge| (&edge.definition, &edge.pipe, &edge.policy))
            .chain(
                publication
                    .desired
                    .unbound_relationships
                    .iter()
                    .map(|edge| (&edge.definition, &edge.pipe, &edge.policy)),
            )
            .filter(|(edge, _, _)| edge.to.component == query_id)
        {
            let spec = publication.desired.specifications.get(&edge.from.component);
            if spec.is_some_and(|spec| spec.implementation == timer) {
                continue;
            }
            let mut native_input = None;
            for (rank, input) in original_config.inputs.iter().enumerate() {
                let pending =
                    ComputationPipelineBuilder::pending_native_endpoint(id, &input.component)?;
                if input.component == edge.from.component.as_str() || edge.from == pending {
                    native_input = Some((rank, &input.component, edge.from != pending));
                    break;
                }
            }
            if let (Some((rank, component, resolved)), DesiredPipe::Ranked(pipe)) =
                (native_input, pipe)
            {
                if pipe.source_id.is_none()
                    && pipe.source_dispatch.is_none()
                    && !pipe.drop_when_full
                    && pipe.source_rank == rank
                    && input_queue.as_ref() == Some(&pipe.queue)
                    && (*policy == RelationshipPolicy::default()
                        || *policy
                            == RelationshipPolicy {
                                activation: ActivationCoupling::RequiresRunning,
                                ..Default::default()
                            })
                {
                    native_sources.insert(component.clone(), resolved.then(|| edge.from.clone()));
                    continue;
                }
            }
            let spec = spec.ok_or_else(|| {
                anyhow::anyhow!(
                "query '{id}' has an externally supplied input; use computation topology operations"
            )
            })?;
            anyhow::ensure!(
                spec.implementation == reader,
                "query '{id}' has custom input connections; use computation topology operations"
            );
            let subscription = publication
                .resource(&dependency(spec, "subscription")?)?
                .get::<LegacySourceSubscription>()?;
            sources.insert(
                subscription.host().id().to_owned(),
                (subscription.host().clone(), subscription.options()),
            );
        }
        let native_only =
            !native_sources.is_empty() && native_sources.len() == original_config.inputs.len();
        let mut native_members: BTreeSet<_> = native_sources
            .values()
            .flatten()
            .map(|output| output.component.clone())
            .collect();
        for source in &config.inputs {
            if let Some(output) = native_sources.remove(&source.component) {
                let component = ComponentId::try_new(source.component.as_str())?;
                let explicit = source.port.as_deref().map(PortId::try_new).transpose()?;
                let output = if publication
                    .desired
                    .nodes
                    .iter()
                    .any(|node| node.descriptor.id() == &component)
                {
                    Some(publication.output_endpoint(&component, explicit.as_ref())?)
                } else {
                    explicit
                        .map(|port| Endpoint::new(component, port))
                        .or(output)
                };
                builder = match output {
                    Some(output) => builder.native_source(output)?,
                    None => builder.pending_native_source(&source.component)?,
                };
                continue;
            }
            let native_output = publication.desired.nodes.iter().any(|node| {
                node.descriptor.id().as_str() == source.component
                    && node
                        .descriptor
                        .ports()
                        .iter()
                        .any(|port| port.direction() == PortDirection::Output)
            });
            if native_output || native_only {
                let component = ComponentId::try_new(source.component.as_str())?;
                let explicit = source.port.as_deref().map(PortId::try_new).transpose()?;
                builder = match (native_output, explicit) {
                    (false, Some(port)) => builder.native_source(Endpoint::new(component, port))?,
                    (false, None) => builder.pending_native_source(&source.component)?,
                    (true, explicit) => builder.native_source(
                        publication.output_endpoint(&component, explicit.as_ref())?,
                    )?,
                };
                continue;
            }
            let (mut host, options) = match sources.remove(&source.component) {
                Some(existing) => existing,
                None => (
                    self.source_host(&source.component).await?,
                    SourceSubscriptionOptions {
                        borrowed_recovery: true,
                        allow_broadcast_loss: true,
                        ..Default::default()
                    },
                ),
            };
            if publication
                .desired
                .subscriptions
                .iter()
                .any(|(from, to)| from.as_str() == source.component && to == &query_id)
            {
                host = self.source_host(&source.component).await?;
            }
            builder = builder.source(host, options)?;
        }
        if native_only && config.enable_bootstrap {
            if let Some(bootstrap) = original
                .dependencies
                .get("bootstrap")
                .and_then(|ids| ids.first())
            {
                let bootstrap = publication
                    .resource(bootstrap)?
                    .get::<QueryBootstrapResource>()?;
                builder = builder.query_bootstrap(id, bootstrap.0.clone())?;
            }
        }
        let indexes_id = dependency(original, "indexes")?;
        if original_config.storage_backend == config.storage_backend
            && publication.desired.resources[&indexes_id].ownership == ResourceOwnership::Borrowed
        {
            let provider = publication
                .resource(&indexes_id)?
                .get::<QueryIndexProviderResource>()?;
            let publication_mode = match original.configuration.get("publication") {
                Some(ConfigurationValue::Literal(value)) if value == "non_atomic" => {
                    QueryPublicationMode::NonAtomic
                }
                _ => QueryPublicationMode::Atomic,
            };
            builder = builder.query_provider(id, provider.0.clone(), publication_mode)?;
        }
        let mut batch = builder.query(config).build()?;
        self.bind_batch_source_dependencies(&mut batch).await?;
        let generated_query = batch
            .definition
            .components
            .iter()
            .find(|node| node.descriptor.id() == &query_id)
            .ok_or_else(|| anyhow::anyhow!("replacement query declaration is missing"))?;
        let ComponentConstruction::Factory(generated_spec) = &generated_query.construction else {
            anyhow::bail!("replacement query requires its factory");
        };
        let remapped = BTreeMap::from([
            (dependency(generated_spec, "catalog")?, catalog_id),
            (dependency(generated_spec, "middleware")?, middleware_id),
        ]);
        let outlets: BTreeSet<_> = batch.definition.components.iter()
            .filter(|node| matches!(&node.construction, ComponentConstruction::Factory(spec) if spec.implementation == outlet))
            .map(|node| node.descriptor.id().clone()).collect();
        batch
            .definition
            .components
            .retain(|node| !outlets.contains(node.descriptor.id()));
        batch
            .definition
            .relationships
            .retain(|edge| !outlets.contains(&edge.definition.to.component));
        batch
            .definition
            .resources
            .retain(|resource| !remapped.contains_key(&resource.id));
        batch
            .bindings
            .resources
            .retain(|resource, _| !remapped.contains_key(resource));
        for node in &mut batch.definition.components {
            if let ComponentConstruction::Factory(spec) = &mut node.construction {
                for resource in spec.dependencies.values_mut().flatten() {
                    if let Some(existing) = remapped.get(resource) {
                        *resource = existing.clone();
                    }
                }
                if node.descriptor.id() == &query_id {
                    spec.configuration.insert(
                        "reset_configuration".into(),
                        ConfigurationValue::Literal(true.into()),
                    );
                }
            }
        }
        let mut members = query_members(&publication, &query_id);
        members.retain(|member, _| {
            publication
                .desired
                .specifications
                .get(member)
                .map_or(true, |spec| spec.implementation != outlet)
        });
        let selected: BTreeSet<_> = members.keys().cloned().collect();
        let mut target = publication.desired.select(GraphSelection::All)?;
        let candidates = publication
            .desired
            .select(GraphSelection::Exact(selected.iter().cloned().collect()))?
            .resources;
        let mut changes = Vec::new();
        target.boundary_relationships.retain(|edge| {
            let affected = selected.contains(&edge.definition.from.component)
                || selected.contains(&edge.definition.to.component);
            let preserved_output = edge.definition.from.component == query_id
                && !selected.contains(&edge.definition.to.component);
            if affected && !preserved_output {
                changes.push(DesiredMutation::Unbind {
                    edge: edge.definition.clone(),
                    policy: RemovalPolicy::Reject,
                });
                false
            } else {
                true
            }
        });
        target.relationships.retain(|edge| {
            let affected = selected.contains(&edge.definition.from.component)
                || selected.contains(&edge.definition.to.component);
            let preserved_output = edge.definition.from.component == query_id
                && !selected.contains(&edge.definition.to.component);
            if affected && !preserved_output {
                changes.push(DesiredMutation::Unbind {
                    edge: edge.definition.clone(),
                    policy: RemovalPolicy::Reject,
                });
                false
            } else {
                true
            }
        });
        let replacement_ids: BTreeSet<_> = batch
            .definition
            .components
            .iter()
            .map(|node| node.descriptor.id().clone())
            .collect();
        let removed: Vec<_> = selected.difference(&replacement_ids).cloned().collect();
        if !removed.is_empty() {
            changes.push(DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(removed),
                policy: RemovalPolicy::Reject,
            });
        }
        target
            .components
            .retain(|node| !selected.contains(node.descriptor.id()));
        target
            .component_resources
            .retain(|node, _| !selected.contains(node));
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
        for consumer in &batch.definition.components {
            let consumer = consumer.descriptor.id();
            changes.push(DesiredMutation::SetSubscriptions {
                consumer: consumer.clone(),
                producers: batch
                    .definition
                    .subscriptions
                    .iter()
                    .filter(|(_, to)| to == consumer)
                    .map(|(from, _)| from.clone())
                    .collect(),
            });
        }
        changes.extend(query_resource_changes(
            &publication,
            &mut target,
            &mut batch,
        )?);
        changes.extend(remove_unused_resources(
            &mut target,
            candidates,
            &BTreeSet::new(),
        )?);
        let was_running = query_activation_requested(&publication, &query_id);
        batch.bindings.defer_activation = true;
        for (source, _) in &batch.definition.subscriptions {
            if let Some(observed) = publication.observed.components.get(source) {
                members.insert(source.clone(), observed.generation);
            }
        }
        native_members.extend(
            batch
                .definition
                .boundary_relationships
                .iter()
                .filter(|edge| {
                    edge.definition.to.component == query_id
                        && !replacement_ids.contains(&edge.definition.from.component)
                })
                .map(|edge| edge.definition.from.component.clone()),
        );
        for source in native_members {
            if let Some(observed) = publication.observed.components.get(&source) {
                members.insert(source, observed.generation);
            }
        }
        Ok(NativeQueryUpdate {
            members,
            changes,
            bindings: batch.bindings,
            running: was_running,
        })
    }
}

pub(crate) struct NativeQuery {
    pub(super) config: QueryConfig,
    pub(super) handle: ComponentHandle,
    pub(super) control: GraphControl,
    pub(super) owner: std::sync::Weak<Runtime>,
}

impl NativeQuery {
    pub(crate) fn inspector(&self) -> ComputationInspector {
        self.control.inspector()
    }

    pub(crate) fn checkpoint_store(
        &self,
    ) -> anyhow::Result<Option<Arc<dyn drasi_core::interface::CheckpointStore>>> {
        self.access()?
            .catalog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("query has no result catalog"))?
            .checkpoint_store(&self.config.id)
    }

    fn status_now(&self) -> ComponentStatus {
        if matches!(
            self.control.state(),
            GraphState::Cancelled
                | GraphState::Completed
                | GraphState::CleanupRequired
                | GraphState::Failed
        ) {
            return ComponentStatus::Stopped;
        }
        self.handle
            .observed()
            .map(|state| super::events::observed_status(&state, ComponentStatus::Added))
            .unwrap_or(ComponentStatus::Stopped)
    }

    fn access(&self) -> anyhow::Result<Arc<QueryApi>> {
        let publication = self.control.registry_snapshot();
        anyhow::ensure!(
            publication
                .observed
                .components
                .get(self.handle.id())
                .is_some_and(|node| node.generation == self.handle.generation()),
            GraphError::StaleGeneration
        );
        let api = publication.query_api(self.handle.id()).ok_or_else(|| {
            anyhow::anyhow!(
                "Query '{}' does not expose result access in its current construction state",
                self.config.id
            )
        })?;
        anyhow::ensure!(
            api.config.id == self.config.id,
            "query read capability belongs to another component"
        );
        Ok(api)
    }

    async fn subscribe_native(&self) -> anyhow::Result<NativeReceiver> {
        self.fetch_snapshot().await?;
        let api = self.access()?;
        let catalog = Runtime::native_result_catalog(
            &self.control.registry_snapshot(),
            &self.config.id,
            &api,
        )?;
        let (receiver, _) = catalog.subscribe_query_at_head(&self.config.id)?;
        self.handle.observed()?;
        Ok(NativeReceiver {
            receiver,
            handle: self.handle.clone(),
            control: self.control.clone(),
        })
    }

    pub(crate) async fn subscribe_results(
        self: &Arc<Self>,
    ) -> anyhow::Result<(
        SnapshotResponse,
        impl futures::Stream<Item = anyhow::Result<Arc<QueryResult>>> + Send + 'static,
    )> {
        let receiver = self.subscribe_native().await?;
        let head = receiver.receiver.head()?;
        let snapshot = self.fetch_snapshot().await?;
        anyhow::ensure!(
            snapshot.output_generation == head.generation
                && snapshot.as_of_sequence >= head.sequence,
            "query output changed while establishing the snapshot subscription"
        );
        let reader = QueryResultReader {
            query: self.clone(),
            receiver,
            generation: head.generation,
            sequence: snapshot.as_of_sequence,
            replay: Vec::new().into_iter(),
        };
        Ok((
            snapshot,
            futures::stream::try_unfold(reader, |mut reader| async move {
                let result = reader.recv().await?;
                Ok(Some((result, reader)))
            }),
        ))
    }

    async fn guard<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T, FetchError>>,
    ) -> Result<T, FetchError> {
        let mut observed = self.control.subscribe_observed();
        tokio::pin!(operation);
        loop {
            let status = self.status_now();
            if !matches!(status, ComponentStatus::Starting | ComponentStatus::Running) {
                return Err(FetchError::NotRunning { status });
            }
            tokio::select! {
                result = &mut operation => {
                    self.handle.observed().map_err(|_| FetchError::NotRunning { status: ComponentStatus::Stopped })?;
                    let status = self.status_now();
                    if !matches!(status, ComponentStatus::Starting | ComponentStatus::Running) {
                        return Err(FetchError::NotRunning { status });
                    }
                    return result;
                }
                result = observed.changed() => {
                    result.map_err(|_| FetchError::NotRunning { status: ComponentStatus::Stopped })?;
                }
            }
        }
    }
}

struct NativeReceiver {
    receiver: super::CatalogSubscription,
    handle: ComponentHandle,
    control: GraphControl,
}

impl NativeReceiver {
    fn require_current(&self) -> anyhow::Result<()> {
        self.handle.observed()?;
        anyhow::ensure!(
            !matches!(
                self.control.state(),
                GraphState::Cancelled
                    | GraphState::Completed
                    | GraphState::Failed
                    | GraphState::CleanupRequired
            ),
            "query output owner is shut down"
        );
        Ok(())
    }
}

struct QueryResultReader {
    query: Arc<NativeQuery>,
    receiver: NativeReceiver,
    generation: u64,
    sequence: u64,
    replay: std::vec::IntoIter<Arc<QueryResult>>,
}

impl QueryResultReader {
    fn require_current(&self) -> anyhow::Result<()> {
        self.receiver.require_current()?;
        anyhow::ensure!(
            self.query.access()?.generation()? == self.generation,
            "query output generation changed; recreate the subscription"
        );
        Ok(())
    }

    async fn recv(&mut self) -> anyhow::Result<Arc<QueryResult>> {
        loop {
            self.require_current()?;
            if let Some(result) = self.replay.next() {
                self.sequence = result.sequence;
                return Ok(result);
            }
            let result = match self.receiver.recv().await {
                Ok(result) => Some(result),
                Err(error)
                    if matches!(
                        error.downcast_ref::<tokio::sync::broadcast::error::RecvError>(),
                        Some(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    ) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            self.require_current()?;
            if let Some(result) = result {
                if result.sequence <= self.sequence {
                    continue;
                }
                if self.sequence.checked_add(1) == Some(result.sequence) {
                    self.sequence = result.sequence;
                    return Ok(result);
                }
            }
            let replay = self.query.fetch_outbox(self.sequence).await?;
            self.require_current()?;
            anyhow::ensure!(
                replay.output_generation == self.generation,
                "query output generation changed during subscription recovery"
            );
            anyhow::ensure!(
                replay.is_contiguous_after(self.sequence),
                "query output history is not contiguous after sequence {}",
                self.sequence
            );
            self.replay = replay.results.into_iter();
        }
    }
}

#[async_trait]
impl crate::channels::ChangeReceiver<crate::channels::QueryResult> for NativeReceiver {
    async fn recv(&mut self) -> anyhow::Result<Arc<crate::channels::QueryResult>> {
        let mut observed = self.control.subscribe_observed();
        loop {
            self.require_current()?;
            tokio::select! {
                result = self.receiver.receive_after_head() => {
                    let envelope = result?;
                    self.require_current()?;
                    return Ok(Arc::new(QueryChangeCodec::to_legacy_result(&envelope)?));
                }
                result = observed.changed() => { result?; }
            }
        }
    }
}

#[async_trait]
impl Query for NativeQuery {
    async fn start(&self) -> anyhow::Result<()> {
        self.owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("instance runtime was dropped"))?
            .start_query_handle(&self.handle)
            .await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.handle.observed()?;
        if self.status_now() == ComponentStatus::Stopped
            && !query_needs_stop(&self.control.registry_snapshot(), self.handle.id())
        {
            return Ok(());
        }
        stop_query(
            &self.control,
            self.handle.id(),
            Some(self.handle.generation()),
        )
        .await
    }
    async fn status(&self) -> ComponentStatus {
        self.status_now()
    }
    fn get_config(&self) -> &QueryConfig {
        &self.config
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn subscription_count(&self) -> usize {
        let publication = self.control.registry_snapshot();
        if !publication
            .observed
            .components
            .get(self.handle.id())
            .is_some_and(|state| state.generation == self.handle.generation())
        {
            return 0;
        }
        let reader = SourcePluginAdapterFactory::default()
            .descriptor()
            .implementation
            .clone();
        let mut resources = BTreeSet::new();
        for member in query_members(&publication, self.handle.id()).keys() {
            let Some(specification) = publication.desired.specifications.get(member) else {
                continue;
            };
            if specification.implementation == reader {
                resources.extend(
                    specification
                        .dependencies
                        .get("subscription")
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
        }
        resources
            .iter()
            .filter(|resource| {
                publication
                    .observed
                    .resources
                    .get(*resource)
                    .is_some_and(|state| state.realization == ResourceRealization::Created)
            })
            .filter(|resource| {
                match publication
                    .resource(resource)
                    .and_then(|handle| handle.get::<LegacySourceSubscription>())
                {
                    Ok(subscription) => subscription.is_subscribed(),
                    Err(error) => {
                        log::warn!(
                            "Could not inspect query '{}' subscription '{}': {error:#}",
                            self.handle.id(),
                            resource
                        );
                        false
                    }
                }
            })
            .count()
    }
    async fn subscribe(&self, _: String) -> anyhow::Result<QuerySubscriptionResponse> {
        let receiver = self.subscribe_native().await?;
        let as_of_sequence = receiver.receiver.head()?.sequence;
        Ok(QuerySubscriptionResponse {
            query_id: self.config.id.clone(),
            receiver: Box::new(receiver),
            as_of_sequence,
        })
    }

    async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
        self.guard(async {
            let api = self.access().map_err(|error| {
                log::warn!("Query result access failed: {error:#}");
                FetchError::NotRunning {
                    status: self.status_now(),
                }
            })?;
            api.snapshot(Duration::from_secs(self.config.bootstrap_timeout_secs))
                .await
                .map_err(|error| {
                    log::warn!("Query snapshot failed: {error:#}");
                    if error
                        .downcast_ref::<tokio::time::error::Elapsed>()
                        .is_some()
                    {
                        FetchError::TimedOut
                    } else {
                        FetchError::NotRunning {
                            status: self.status_now(),
                        }
                    }
                })
        })
        .await
    }
    async fn fetch_outbox(&self, after: u64) -> Result<OutboxResponse, FetchError> {
        self.guard(async {
            let api = self.access().map_err(|error| {
                log::warn!("Query result access failed: {error:#}");
                FetchError::NotRunning {
                    status: self.status_now(),
                }
            })?;
            api.outbox(
                after,
                Duration::from_secs(self.config.bootstrap_timeout_secs),
            )
            .await
        })
        .await
    }
    async fn output_generation(&self) -> u64 {
        match self.access().and_then(|api| api.generation()) {
            Ok(generation) => generation,
            Err(error) => {
                log::warn!("Query generation is unavailable: {error:#}");
                0
            }
        }
    }
    fn output_metrics(&self) -> Option<Arc<QueryOutputMetrics>> {
        match self.access() {
            Ok(api) => Some(api.metrics.clone()),
            Err(error) => {
                log::warn!("Query metrics are unavailable: {error:#}");
                None
            }
        }
    }
    fn is_volatile(&self) -> bool {
        match self.access().and_then(|api| api.is_persistent()) {
            Ok(persistent) => !persistent,
            Err(error) => {
                log::warn!("Cannot establish persistent query output: {error:#}");
                true
            }
        }
    }

    async fn release_persistent_handles(&self) {
        if !self.is_volatile() {
            log::error!(
                "Query '{}' storage is owned by its computation component; use awaited instance shutdown or component-resource removal to release it",
                self.config.id
            );
        }
    }
}
