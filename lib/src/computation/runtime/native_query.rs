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
    channels::QuerySubscriptionResponse,
    computation::v1::*,
    config::QueryConfig,
    metrics::QueryOutputMetrics,
    queries::{FetchError, OutboxResponse, Query, SnapshotResponse},
    ComponentStatus,
};
use async_trait::async_trait;

fn query_members(
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

pub(super) async fn start_query(
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
    let report = control
        .start_members(query_members(&publication, id))
        .await?;
    anyhow::ensure!(
        report.summary == OperationSummary::Completed,
        "query startup is incomplete: {report:?}"
    );
    Ok(())
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
    anyhow::ensure!(
        matches!(
            state.lifecycle,
            ComponentLifecycle::Running | ComponentLifecycle::Starting | ComponentLifecycle::Failed
        ) || state.failure.is_some()
            || matches!(
                state.realization,
                RealizationState::Pending | RealizationState::Creating
            ),
        "Query '{id}' cannot stop while it is {:?}",
        state.lifecycle
    );
    let report = control
        .stop_members(query_members(&publication, id))
        .await?;
    anyhow::ensure!(
        report.summary == OperationSummary::Completed,
        "query stop is incomplete: {report:?}"
    );
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

fn resource_users(topology: &DesiredTopology, resource: &ResourceId) -> BTreeSet<ComponentId> {
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

pub(super) struct NativeQueryUpdate {
    pub(super) members: BTreeMap<ComponentId, ComponentGeneration>,
    pub(super) changes: Vec<DesiredMutation>,
    pub(super) bindings: TopologyBindings,
    running: bool,
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
        for resource in candidates {
            if resource_users(&target, &resource.id).is_empty() {
                changes.push(DesiredMutation::RemoveResource {
                    resource: resource.id,
                    policy: RemovalPolicy::Reject,
                });
            }
        }
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
        self.logs
            .remove_component_by_key(&crate::managers::ComponentLogKey::new(
                self.config.id.clone(),
                match kind {
                    "source" => crate::channels::ComponentType::Source,
                    "query" => crate::channels::ComponentType::Query,
                    _ => crate::channels::ComponentType::Reaction,
                },
                id,
            ))
            .await;
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
            start_query(&control, &ComponentId::try_new(id)?, None).await?;
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
        .with_catalog(catalog.as_ref().clone());
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
        for edge in publication
            .desired
            .edges
            .iter()
            .filter(|edge| edge.definition.to.component == query_id)
        {
            let spec = publication.desired.specifications.get(&edge.definition.from.component)
                .ok_or_else(|| anyhow::anyhow!("query '{id}' has an externally supplied input; use computation topology operations"))?;
            if spec.implementation == timer {
                continue;
            }
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
        for source in &config.sources {
            let (mut host, options) = match sources.remove(&source.source_id) {
                Some(existing) => existing,
                None => (
                    self.source_host(&source.source_id).await?,
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
                .any(|(from, to)| from.as_str() == source.source_id && to == &query_id)
            {
                host = self.source_host(&source.source_id).await?;
            }
            builder = builder.source(host, options)?;
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
        changes.extend(
            batch
                .definition
                .relationships
                .iter()
                .cloned()
                .map(DesiredMutation::Bind),
        );
        let producers = batch
            .definition
            .subscriptions
            .iter()
            .filter(|(_, to)| to == &query_id)
            .map(|(from, _)| from.clone())
            .collect();
        changes.push(DesiredMutation::SetSubscriptions {
            consumer: query_id.clone(),
            producers,
        });
        for resource in &batch.definition.resources {
            let unchanged = publication.desired.resources.get(&resource.id) == Some(resource)
                && batch
                    .bindings
                    .resources
                    .get(&resource.id)
                    .is_some_and(|supplied| {
                        publication
                            .resource(&resource.id)
                            .is_ok_and(|existing| existing.same_shared_instance(supplied))
                    });
            if unchanged {
                batch.bindings.resources.remove(&resource.id);
            } else {
                changes.push(DesiredMutation::PutResource(resource.clone()));
                if publication.desired.resources.contains_key(&resource.id) {
                    changes.push(DesiredMutation::RebindResource(resource.id.clone()));
                }
            }
        }
        for resource in candidates {
            if resource_users(&target, &resource.id).is_empty() {
                changes.push(DesiredMutation::RemoveResource {
                    resource: resource.id,
                    policy: RemovalPolicy::Reject,
                });
            }
        }
        let was_running = publication
            .observed
            .components
            .get(&query_id)
            .is_some_and(|node| {
                matches!(
                    node.lifecycle,
                    ComponentLifecycle::Running | ComponentLifecycle::Starting
                )
            });
        batch.bindings.defer_activation = true;
        for (source, _) in &batch.definition.subscriptions {
            let generation = publication
                .observed
                .components
                .get(source)
                .ok_or(GraphError::StaleGeneration)?
                .generation;
            members.insert(source.clone(), generation);
        }
        Ok(NativeQueryUpdate {
            members,
            changes,
            bindings: batch.bindings,
            running: was_running,
        })
    }
}

pub(super) struct NativeQuery {
    pub(super) config: QueryConfig,
    pub(super) handle: ComponentHandle,
    pub(super) control: GraphControl,
}

impl NativeQuery {
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

#[async_trait]
impl crate::channels::ChangeReceiver<crate::channels::QueryResult> for NativeReceiver {
    async fn recv(&mut self) -> anyhow::Result<Arc<crate::channels::QueryResult>> {
        let mut observed = self.control.subscribe_observed();
        loop {
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
            tokio::select! {
                result = self.receiver.receive() => {
                    let envelope = result?;
                    self.handle.observed()?;
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
        start_query(
            &self.control,
            self.handle.id(),
            Some(self.handle.generation()),
        )
        .await
    }
    async fn stop(&self) -> anyhow::Result<()> {
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
        if matches!(
            self.status_now(),
            ComponentStatus::Running | ComponentStatus::Starting
        ) {
            self.config.sources.len()
        } else {
            0
        }
    }
    async fn subscribe(&self, _: String) -> anyhow::Result<QuerySubscriptionResponse> {
        self.fetch_snapshot().await?;
        let api = self.access()?;
        let catalog = Runtime::native_result_catalog(
            &self.control.registry_snapshot(),
            &self.config.id,
            &api,
        )?;
        let (receiver, as_of_sequence) = catalog.subscribe_query_at_head(&self.config.id)?;
        self.handle.observed()?;
        Ok(QuerySubscriptionResponse {
            query_id: self.config.id.clone(),
            receiver: Box::new(NativeReceiver {
                receiver,
                handle: self.handle.clone(),
                control: self.control.clone(),
            }),
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
