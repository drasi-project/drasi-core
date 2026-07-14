// Copyright 2025 The Drasi Authors.
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

use anyhow::Result;
use log::info;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::component_graph::{ComponentGraph, ComponentKind, ComponentUpdateSender};
use crate::config::ReactionRuntime;
use crate::consumption::{ConsumptionEngine, QueryConsumer, ReactionConsumer};
use crate::context::ReactionRuntimeContext;
use crate::identity::IdentityProvider;
use crate::managers::{ComponentLogKey, ComponentLogRegistry};
use crate::metrics::{LifecycleMetrics, StartupRejectionReason};
use crate::reactions::snapshot_fetcher::InProcessSnapshotFetcher;
use crate::reactions::{QueryProvider, Reaction};
use crate::recovery::ReactionRecoveryPolicy;
use crate::state_store::StateStoreProvider;

pub struct ReactionManager {
    instance_id: String,
    /// Query provider for reactions to access queries (injected after DrasiLib is constructed)
    query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
    /// State store provider for reactions to persist state
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Identity provider for credential injection
    identity_provider: Arc<RwLock<Option<Arc<dyn IdentityProvider>>>>,
    /// Log registry for component log streaming
    log_registry: Arc<ComponentLogRegistry>,
    /// Shared component graph — the single source of truth for component metadata,
    /// state, relationships, runtime instances, AND event history.
    graph: Arc<RwLock<ComponentGraph>>,
    /// Channel sender for routing status updates through the graph update loop.
    /// Managers send transitional states (Starting, Stopping, Reconfiguring) here;
    /// the loop applies them to the graph and records events automatically.
    update_tx: ComponentUpdateSender,
    engine: ConsumptionEngine,
}

impl ReactionManager {
    /// Create a new ReactionManager
    ///
    /// # Parameters
    /// - `instance_id`: The DrasiLib instance ID for log routing
    /// - `log_registry`: Shared log registry for component log streaming
    /// - `graph`: Shared component graph for tracking component relationships and emitting events
    pub fn new(
        instance_id: impl Into<String>,
        log_registry: Arc<ComponentLogRegistry>,
        graph: Arc<RwLock<ComponentGraph>>,
        update_tx: ComponentUpdateSender,
    ) -> Self {
        let instance_id = instance_id.into();
        let query_provider = Arc::new(RwLock::new(None));
        let state_store = Arc::new(RwLock::new(None));
        let subscription_tasks = Arc::new(RwLock::new(HashMap::new()));
        let metrics = Arc::new(RwLock::new(HashMap::new()));
        let lifecycle_metrics = Arc::new(LifecycleMetrics::new());
        let engine = ConsumptionEngine::new(
            instance_id.clone(),
            graph.clone(),
            query_provider.clone(),
            state_store.clone(),
            subscription_tasks.clone(),
            metrics,
            lifecycle_metrics,
        );

        Self {
            instance_id,
            query_provider,
            state_store,
            identity_provider: Arc::new(RwLock::new(None)),
            log_registry,
            graph,
            update_tx,
            engine,
        }
    }

    /// Inject the query provider (called after DrasiLib is fully constructed)
    ///
    /// This allows the ReactionManager to provide query access to reactions.
    pub async fn inject_query_provider(&self, qp: Arc<dyn QueryProvider>) {
        self.engine.inject_query_provider(qp).await;
    }

    /// Inject the state store provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to access the state store when they are added.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        self.engine.inject_state_store(state_store).await;
    }

    /// Inject the identity provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to obtain authentication credentials when they are added.
    pub async fn inject_identity_provider(&self, identity_provider: Arc<dyn IdentityProvider>) {
        *self.identity_provider.write().await = Some(identity_provider);
    }

    /// Add a reaction instance, taking ownership and wrapping it in an Arc internally.
    ///
    /// This method handles runtime-only operations: creating the runtime context,
    /// initializing the reaction, and storing it in the runtime map. Graph registration
    /// (node creation, dependency edges) must be done by the caller beforehand via
    /// `ComponentGraph::register_reaction()`.
    ///
    /// # Parameters
    /// - `reaction`: The reaction instance to provision (ownership is transferred)
    ///
    /// # Note
    /// The reaction will NOT be auto-started. Call `start_reaction` separately
    /// if you need to start it after adding.
    pub async fn provision_reaction(&self, reaction: impl Reaction + 'static) -> Result<()> {
        let reaction: Arc<dyn Reaction> = Arc::new(reaction);
        let reaction_id = reaction.id().to_string();

        // Build snapshot fetcher scoped to this reaction's query IDs
        let snapshot_fetcher = Arc::new(InProcessSnapshotFetcher::new(
            self.query_provider.clone(),
            reaction.query_ids(),
        ));

        // Construct runtime context for this reaction
        let mut context = ReactionRuntimeContext::new(
            &self.instance_id,
            &reaction_id,
            self.state_store.read().await.clone(),
            self.update_tx.clone(),
            None,
        );
        context.identity_provider = self.identity_provider.read().await.clone();
        context.snapshot_fetcher = Some(snapshot_fetcher);

        // Initialize the reaction with its runtime context
        reaction.initialize(context).await;

        // Store the runtime instance in the graph
        {
            let mut graph = self.graph.write().await;
            graph.set_runtime(&reaction_id, Box::new(reaction))?;
        }

        info!("Provisioned reaction: {reaction_id}");

        Ok(())
    }

    /// Start a reaction.
    ///
    /// The reaction must have been added via `add_reaction()` first, which injects
    /// the necessary dependencies (event channel and query subscriber).
    ///
    /// Performs startup validation (§3), then:
    /// 1. Starts the reaction (spawns processing loop which waits on bootstrap gate)
    /// 2. Wires live subscriptions (events buffer in priority queue)
    /// 3. Runs per-query bootstrap (§5) — reads checkpoints, fetches snapshot/outbox,
    ///    applies recovery policy, invokes `bootstrap()` hook if needed
    /// 4. Opens the bootstrap gate — processing loop begins draining
    ///
    /// # Parameters
    /// - `id`: The reaction ID to start
    pub async fn start_reaction(&self, id: String) -> Result<()> {
        let reaction =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Reaction>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new(
                        "reaction", &id,
                    ))
                })?;

        // --- §3 Startup validation ---
        self.validate_startup_config(&reaction).await?;

        crate::managers::lifecycle_helpers::start_component(
            &self.graph,
            &id,
            "reaction",
            &reaction,
        )
        .await?;

        // Create the bootstrap gate — forwarders wait on this before processing.
        // Using a watch channel (not Notify) so late subscribers see the current value
        // and cannot miss the notification.
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);

        let consumer = Arc::new(ReactionConsumer(reaction.clone())) as Arc<dyn QueryConsumer>;
        if let Err(e) = self
            .engine
            .subscribe_and_bootstrap(&id, consumer, gate_rx)
            .await
        {
            // Abort any forwarder/supervisor tasks spawned during wire_subscriptions.
            self.engine.abort_subscription_tasks(&id).await;

            // Revert to Error since the reaction can't receive data without subscriptions
            let mut graph = self.graph.write().await;
            let _ = graph.validate_and_transition(
                &id,
                ComponentStatus::Error,
                Some(format!("Bootstrap failed: {e}")),
            );
            return Err(e);
        }

        // Open the gate — forwarders begin draining buffered events.
        let _ = gate_tx.send(true);

        Ok(())
    }

    /// Validate the reaction's startup configuration (§3 compatibility rules).
    ///
    /// 1. `is_durable=true` requires a durable state store
    /// 2. `needs_snapshot_on_fresh_start=true` + `AutoSkipGap` → reject (contradictory)
    /// 3. `needs_snapshot_on_fresh_start=false` + `AutoReset` → reject (AutoReset needs
    ///    snapshot capability to re-bootstrap)
    async fn validate_startup_config(&self, reaction: &Arc<dyn Reaction>) -> Result<()> {
        let is_durable = reaction.is_durable();
        let needs_snapshot = reaction.needs_snapshot_on_fresh_start();
        let policy = reaction.default_recovery_policy();

        // Rule 1: durable reaction requires durable state store
        if is_durable {
            let store = self.state_store.read().await;
            match store.as_ref() {
                None => {
                    self.engine
                        .lifecycle_metrics()
                        .record_startup_rejection(StartupRejectionReason::DurableNoStore);
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' requires a durable state store (is_durable=true), \
                         but no state store is configured",
                        reaction.id()
                    ));
                }
                Some(s) if !s.is_durable() => {
                    self.engine
                        .lifecycle_metrics()
                        .record_startup_rejection(StartupRejectionReason::DurableOnVolatile);
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' requires a durable state store (is_durable=true), \
                         but the configured state store is volatile",
                        reaction.id()
                    ));
                }
                _ => {}
            }
        }

        // Rule 2: snapshot + AutoSkipGap is contradictory
        if needs_snapshot && policy == ReactionRecoveryPolicy::AutoSkipGap {
            self.engine
                .lifecycle_metrics()
                .record_startup_rejection(StartupRejectionReason::SnapshotSkipGap);
            return Err(anyhow::anyhow!(
                "Reaction '{}': needs_snapshot_on_fresh_start=true is incompatible with \
                 AutoSkipGap recovery policy (cannot skip gap if snapshot is required)",
                reaction.id()
            ));
        }

        // Rule 3: !snapshot + AutoReset is contradictory
        if !needs_snapshot && policy == ReactionRecoveryPolicy::AutoReset {
            self.engine
                .lifecycle_metrics()
                .record_startup_rejection(StartupRejectionReason::NoSnapshotAutoReset);
            return Err(anyhow::anyhow!(
                "Reaction '{}': needs_snapshot_on_fresh_start=false is incompatible with \
                 AutoReset recovery policy (AutoReset re-bootstraps from a snapshot)",
                reaction.id()
            ));
        }

        Ok(())
    }

    /// Stop a running reaction and abort its subscription forwarder tasks.
    ///
    /// # Errors
    /// Returns an error if the reaction is not found or the stop operation fails.
    pub async fn stop_reaction(&self, id: String) -> Result<()> {
        let reaction =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Reaction>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new(
                        "reaction", &id,
                    ))
                })?;

        // Transition to Stopping FIRST so the supervisor won't race us
        // and transition to Error when forwarder tasks are aborted.
        {
            let mut g = self.graph.write().await;
            g.validate_and_transition(
                &id,
                ComponentStatus::Stopping,
                Some("Stopping reaction".to_string()),
            )?;
        }

        // Now abort subscription forwarder tasks
        self.abort_subscription_tasks(&id).await;

        // Call the reaction's stop logic
        if let Err(e) = reaction.stop().await {
            let mut g = self.graph.write().await;
            let _ = g.validate_and_transition(
                &id,
                ComponentStatus::Error,
                Some(format!("Stop failed: {e}")),
            );
            return Err(e);
        }

        Ok(())
    }

    /// Returns the current status of a reaction (e.g. Running, Stopped, Error).
    ///
    /// # Errors
    /// Returns an error if the reaction is not found.
    pub async fn get_reaction_status(&self, id: String) -> Result<ComponentStatus> {
        crate::managers::lifecycle_helpers::get_component_status(&self.graph, &id, "Reaction").await
    }

    /// Retrieve a reaction's runtime descriptor, including its status, subscribed queries, and properties.
    ///
    /// # Errors
    /// Returns an error if the reaction is not found.
    pub async fn get_reaction(&self, id: String) -> Result<ReactionRuntime> {
        let graph = self.graph.read().await;
        let reaction = graph.get_runtime::<Arc<dyn Reaction>>(&id).cloned();

        if let Some(reaction) = reaction {
            let status = graph
                .get_component(&id)
                .map(|n| n.status)
                .unwrap_or(ComponentStatus::Stopped);
            let error_message = match &status {
                ComponentStatus::Error => graph.get_last_error(&id),
                _ => None,
            };
            drop(graph);
            let runtime = ReactionRuntime {
                id: reaction.id().to_string(),
                reaction_type: reaction.type_name().to_string(),
                status,
                error_message,
                queries: reaction.query_ids(),
                properties: reaction.properties(),
            };
            Ok(runtime)
        } else {
            Err(crate::managers::ComponentNotFoundError::new("reaction", &id).into())
        }
    }

    /// Teardown a reaction's runtime state — stop, deprovision, and remove from runtime map.
    ///
    /// This method handles runtime-only operations. Graph deregistration
    /// (node removal, edge cleanup) must be done by the caller afterwards via
    /// `ComponentGraph::deregister()`.
    ///
    /// The caller should validate dependencies via `graph.can_remove()` before calling this.
    pub async fn teardown_reaction(&self, id: String, cleanup: bool) -> Result<()> {
        let id_clone = id.clone();
        let engine = self.engine.clone();
        crate::managers::lifecycle_helpers::teardown_component::<Arc<dyn Reaction>, _, _>(
            &self.graph,
            &id,
            "reaction",
            ComponentType::Reaction,
            &self.instance_id,
            &self.log_registry,
            cleanup,
            || engine.abort_subscription_tasks(&id_clone),
        )
        .await?;

        // Also abort any remaining subscription tasks after teardown
        self.abort_subscription_tasks(&id).await;

        // Remove metrics entries for this reaction to prevent unbounded memory growth
        {
            let mut metrics_map = self.engine.metrics().write().await;
            metrics_map.retain(|(rid, _), _| rid != &id);
        }

        Ok(())
    }

    /// Update a reaction by replacing it with a new instance.
    ///
    /// Flow: validate exists → validate status → set Reconfiguring via graph →
    /// stop if running/starting → wait for stopped → initialize new →
    /// replace (if still exists) → restart if was running.
    /// Log and event history are preserved.
    pub async fn update_reaction(
        &self,
        id: String,
        new_reaction: impl Reaction + 'static,
    ) -> Result<()> {
        let old_reaction = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Reaction>>(&id).cloned()
        };

        if let Some(old_reaction) = old_reaction {
            // Verify the new reaction has the same ID
            if new_reaction.id() != id {
                return Err(anyhow::anyhow!(
                    "New reaction ID '{}' does not match existing reaction ID '{}'",
                    new_reaction.id(),
                    id
                ));
            }

            let graph = &self.graph;
            let instance_id = &self.instance_id;
            let state_store = &self.state_store;
            let update_tx = &self.update_tx;

            crate::managers::lifecycle_helpers::reconfigure_component::<Arc<dyn Reaction>, _, _, _>(
                graph,
                &id,
                "reaction",
                &old_reaction,
                || self.abort_subscription_tasks(&id),
                || async {
                    let new_reaction: Arc<dyn Reaction> = Arc::new(new_reaction);
                    let snapshot_fetcher = Arc::new(InProcessSnapshotFetcher::new(
                        self.query_provider.clone(),
                        new_reaction.query_ids(),
                    ));
                    let mut context = ReactionRuntimeContext::new(
                        instance_id,
                        &id,
                        state_store.read().await.clone(),
                        update_tx.clone(),
                        None,
                    );
                    context.snapshot_fetcher = Some(snapshot_fetcher);
                    new_reaction.initialize(context).await;

                    let mut g = graph.write().await;
                    if !g.has_runtime(&id) {
                        return Err(anyhow::anyhow!(
                            "Reaction '{id}' was concurrently deleted during reconfiguration"
                        ));
                    }
                    g.set_runtime(&id, Box::new(new_reaction))?;
                    Ok(())
                },
                || self.start_reaction(id.clone()),
            )
            .await
        } else {
            Err(crate::managers::ComponentNotFoundError::new("reaction", &id).into())
        }
    }

    /// List all registered reactions with their current statuses.
    pub async fn list_reactions(&self) -> Vec<(String, ComponentStatus)> {
        crate::managers::lifecycle_helpers::list_components(&self.graph, &ComponentKind::Reaction)
            .await
    }

    /// Start all reactions that have `auto_start` enabled.
    ///
    /// Reactions must have been added via `add_reaction()` first, which injects
    /// the necessary dependencies (event channel and query subscriber).
    ///
    /// Only reactions with `auto_start() == true` will be started.
    pub async fn start_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::start_all_components::<Arc<dyn Reaction>, _, _>(
            &self.graph,
            &ComponentKind::Reaction,
            "reaction",
            |r| r.auto_start(),
            |id, _reaction| self.start_reaction(id),
        )
        .await
    }

    /// Stop all currently running reactions.
    ///
    /// # Errors
    /// Returns an error if any reaction fails to stop.
    pub async fn stop_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::stop_all_components(
            &self.graph,
            &ComponentKind::Reaction,
            "Reaction",
            |id| self.stop_reaction(id),
        )
        .await
    }

    /// Record a component event in the history.
    ///
    /// This should be called by the event processing loop to track component
    /// lifecycle events for later querying.
    pub async fn record_event(&self, event: ComponentEvent) {
        let mut graph = self.graph.write().await;
        graph.record_event(event);
    }

    /// Get events for a specific reaction.
    ///
    /// Returns events in chronological order (oldest first).
    pub async fn get_reaction_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.graph.read().await.get_events(id)
    }

    /// Get all events across all reactions.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        let graph = self.graph.read().await;
        graph
            .get_all_events()
            .into_iter()
            .filter(|e| e.component_type == ComponentType::Reaction)
            .collect()
    }

    /// Subscribe to live logs for a reaction.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// Returns None if the reaction doesn't exist.
    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        // Verify the reaction exists in the graph
        {
            let graph = self.graph.read().await;
            if !graph.has_runtime(id) {
                return None;
            }
        }

        let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Reaction, id);
        Some(self.log_registry.subscribe_by_key(&log_key).await)
    }

    /// Subscribe to live events for a reaction.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// Returns None if the reaction doesn't exist.
    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> Option<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let graph = self.graph.read().await;
        if !graph.has_runtime(id) {
            return None;
        }
        graph.subscribe_events(id)
    }

    /// Abort all subscription forwarder tasks for a reaction.
    async fn abort_subscription_tasks(&self, reaction_id: &str) {
        self.engine.abort_subscription_tasks(reaction_id).await;
    }

    /// Get the per-(reaction, query) metrics for a specific reaction.
    ///
    /// Returns a map from query_id to its metrics snapshot.
    ///
    /// # Errors
    /// Returns an error if the reaction does not exist.
    pub async fn get_reaction_metrics(
        &self,
        reaction_id: &str,
    ) -> Result<HashMap<String, crate::metrics::ReactionMetricsSnapshot>> {
        // Verify the reaction exists in the component graph
        let graph = self.graph.read().await;
        if graph
            .get_runtime::<Arc<dyn Reaction>>(reaction_id)
            .is_none()
        {
            return Err(anyhow::anyhow!("Reaction '{reaction_id}' not found",));
        }
        drop(graph);

        let metrics = self.engine.metrics().read().await;
        Ok(metrics
            .iter()
            .filter(|((rid, _), _)| rid == reaction_id)
            .map(|((_, qid), m)| (qid.clone(), m.snapshot()))
            .collect())
    }

    /// Get the global lifecycle metrics.
    pub fn lifecycle_metrics(&self) -> &Arc<LifecycleMetrics> {
        self.engine.lifecycle_metrics()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::QuerySubscriptionResponse;
    use crate::component_graph::ComponentStatusHandle;
    use crate::config::schema::QueryConfig;
    use crate::consumption::engine::{BroadcastGapContext, ConsumptionEngine};
    use crate::consumption::{QueryConsumer, ReactionConsumer};
    use crate::metrics::ReactionMetrics;
    use crate::queries::output_state::{FetchError, OutboxResponse, SnapshotResponse};
    use crate::reactions::checkpoint::ReactionCheckpoint;
    use crate::sources::tests::TestMockSource;
    use async_trait::async_trait;
    use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    // ========================================================================
    // Mock Query for direct unit tests
    // ========================================================================

    /// A mock Query that returns canned snapshot/outbox responses.
    struct MockQuery {
        config: QueryConfig,
        snapshot: tokio::sync::RwLock<SnapshotResponse>,
        outbox_response: tokio::sync::RwLock<Result<OutboxResponse, FetchError>>,
    }

    impl MockQuery {
        fn new(config_hash: u64, snapshot_seq: u64) -> Self {
            let config = QueryConfig {
                id: "q1".to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: crate::config::schema::QueryLanguage::Cypher,
                middleware: vec![],
                sources: vec![],
                auto_start: true,
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
                recovery_policy: None,
                bootstrap_timeout_secs: 300,
                outbox_capacity: 1000,
            };
            let snapshot = SnapshotResponse::new(im::HashMap::new(), snapshot_seq, config_hash);
            Self {
                config,
                snapshot: tokio::sync::RwLock::new(snapshot),
                outbox_response: tokio::sync::RwLock::new(Ok(OutboxResponse {
                    results: vec![],
                    latest_sequence: snapshot_seq,
                    config_hash,
                })),
            }
        }
    }

    #[async_trait]
    impl crate::queries::Query for MockQuery {
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Running
        }
        fn get_config(&self) -> &QueryConfig {
            &self.config
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn subscribe(&self, _reaction_id: String) -> Result<QuerySubscriptionResponse> {
            Err(anyhow::anyhow!("MockQuery does not support subscribe"))
        }
        async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
            Ok(self.snapshot.read().await.clone())
        }
        async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxResponse, FetchError> {
            self.outbox_response.read().await.clone()
        }
    }

    // ========================================================================
    // Configurable mock reaction for testing startup validation and bootstrap
    // ========================================================================

    struct MockReaction {
        id: String,
        queries: Vec<String>,
        durable: bool,
        snapshot_on_fresh: bool,
        policy: ReactionRecoveryPolicy,
        status_handle: ComponentStatusHandle,
        enqueued: Arc<Mutex<Vec<QueryResult>>>,
        bootstrap_count: Arc<AtomicUsize>,
    }

    impl MockReaction {
        fn new(id: &str, queries: Vec<String>) -> Self {
            Self {
                id: id.to_string(),
                queries,
                durable: false,
                snapshot_on_fresh: false,
                policy: ReactionRecoveryPolicy::Strict,
                status_handle: ComponentStatusHandle::new(id),
                enqueued: Arc::new(Mutex::new(Vec::new())),
                bootstrap_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_durable(mut self, v: bool) -> Self {
            self.durable = v;
            self
        }

        fn with_snapshot_on_fresh(mut self, v: bool) -> Self {
            self.snapshot_on_fresh = v;
            self
        }

        fn with_policy(mut self, p: ReactionRecoveryPolicy) -> Self {
            self.policy = p;
            self
        }
    }

    #[async_trait]
    impl Reaction for MockReaction {
        fn id(&self) -> &str {
            &self.id
        }
        fn type_name(&self) -> &str {
            "test-mock"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }
        fn auto_start(&self) -> bool {
            false
        }
        async fn initialize(&self, ctx: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(ctx.update_tx.clone()).await;
        }
        async fn start(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, None)
                .await;
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, None)
                .await;
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }
        fn is_durable(&self) -> bool {
            self.durable
        }
        fn needs_snapshot_on_fresh_start(&self) -> bool {
            self.snapshot_on_fresh
        }
        fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
            self.policy
        }
        async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
            self.enqueued.lock().await.push(result);
            Ok(())
        }
        async fn bootstrap(
            &self,
            _ctx: crate::reactions::bootstrap_context::BootstrapContext,
        ) -> Result<()> {
            self.bootstrap_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // ========================================================================
    // Test helpers
    // ========================================================================

    /// Build a DrasiLib with a source and one query (auto_start=false).
    async fn build_core() -> crate::DrasiLib {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        crate::DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .with_query(
                crate::Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap()
    }

    /// Build a DrasiLib with auto-start query and an injected state store.
    async fn build_core_with_store(
        store: Arc<crate::state_store::MemoryStateStoreProvider>,
    ) -> crate::DrasiLib {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        crate::DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .with_query(
                crate::Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(true)
                    .build(),
            )
            .with_state_store_provider(store)
            .build()
            .await
            .unwrap()
    }

    /// Helper: create a SourceChange::Insert for a Test node.
    fn make_test_insert(
        source_id: &str,
        node_id: &str,
        val: i32,
    ) -> drasi_core::models::SourceChange {
        let mut props = ElementPropertyMap::default();
        props.insert("val", drasi_core::models::ElementValue::Integer(val.into()));
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, node_id),
                labels: vec!["Test".into()].into(),
                effective_from: 1000,
            },
            properties: props,
        };
        drasi_core::models::SourceChange::Insert { element }
    }

    /// Inject events into a DrasiLib's source via downcast.
    async fn inject_events(core: &crate::DrasiLib, count: usize) {
        let source_arc = core
            .source_manager
            .get_source_instance("src1")
            .await
            .expect("Source 'src1' not found");
        let mock_source = source_arc
            .as_any()
            .downcast_ref::<TestMockSource>()
            .expect("Source is not TestMockSource");
        for i in 0..count {
            let change = make_test_insert("src1", &format!("node_{i}"), i as i32);
            mock_source.inject_event(change).await.unwrap();
        }
        // Give the query time to process events.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // ========================================================================
    // §3 Startup validation tests
    // ========================================================================

    #[tokio::test]
    async fn validation_rejects_durable_without_durable_store() {
        let core = build_core().await;
        core.start().await.unwrap();

        // Default store is MemoryStateStoreProvider which is NOT durable.
        let reaction = MockReaction::new("r1", vec!["q1".into()]).with_durable(true);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r1").await;
        assert!(
            result.is_err(),
            "Expected error for durable reaction without durable store"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("durable"),
            "Error should mention 'durable': {msg}"
        );
    }

    #[tokio::test]
    async fn validation_rejects_snapshot_with_auto_skip_gap() {
        let core = build_core().await;
        core.start().await.unwrap();

        let reaction = MockReaction::new("r2", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::AutoSkipGap);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r2").await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("AutoSkipGap") || msg.contains("incompatible"),
            "Error should mention incompatibility: {msg}"
        );
    }

    #[tokio::test]
    async fn validation_rejects_no_snapshot_with_auto_reset() {
        let core = build_core().await;
        core.start().await.unwrap();

        let reaction = MockReaction::new("r3", vec!["q1".into()])
            .with_snapshot_on_fresh(false)
            .with_policy(ReactionRecoveryPolicy::AutoReset);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r3").await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("AutoReset") || msg.contains("incompatible"),
            "Error should mention incompatibility: {msg}"
        );
    }

    #[tokio::test]
    async fn validation_allows_non_durable_no_snapshot_strict() {
        let core = build_core().await;
        core.start().await.unwrap();

        let reaction = MockReaction::new("r4", vec!["q1".into()]);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r4").await;
        assert!(result.is_ok(), "Expected success: {:?}", result.err());
    }

    // ========================================================================
    // Fresh start tests
    // ========================================================================

    #[tokio::test]
    async fn fresh_start_no_snapshot_starts_at_seq_zero() {
        let core = build_core().await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();

        let reaction = MockReaction::new("r5", vec!["q1".into()]);
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r5").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r5",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = core.get_reaction_status("r5").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);
    }

    // ========================================================================
    // §5 Config-hash mismatch + outbox catch-up tests
    // ========================================================================

    #[tokio::test]
    async fn config_hash_mismatch_with_auto_reset_triggers_bootstrap() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Pre-write a checkpoint with a WRONG config hash.
        let wrong_cp = ReactionCheckpoint {
            sequence: 0,
            config_hash: 99999,
        };
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), "r_reset", "q1", &wrong_cp)
            .await
            .unwrap();

        // Add reaction with AutoReset + needs_snapshot.
        let reaction = MockReaction::new("r_reset", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::AutoReset);
        let bc = reaction.bootstrap_count.clone();
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r_reset").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r_reset",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Verify: bootstrap was called (AutoReset re-bootstraps).
        assert!(
            bc.load(Ordering::SeqCst) > 0,
            "Expected bootstrap() to be called on config-hash mismatch with AutoReset"
        );

        // Verify: checkpoint should now have the correct config hash.
        let cp = crate::reactions::checkpoint::read_checkpoint(store.as_ref(), "r_reset", "q1")
            .await
            .unwrap();
        assert!(cp.is_some(), "Checkpoint should exist after AutoReset");
        assert_ne!(
            cp.unwrap().config_hash,
            99999,
            "Checkpoint hash should have been updated from the wrong value"
        );
    }

    #[tokio::test]
    async fn config_hash_mismatch_with_strict_fails_startup() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Pre-write checkpoint with wrong hash.
        let wrong_cp = ReactionCheckpoint {
            sequence: 0,
            config_hash: 99999,
        };
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), "r_strict", "q1", &wrong_cp)
            .await
            .unwrap();

        // Strict policy should reject on hash mismatch.
        let reaction = MockReaction::new("r_strict", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::Strict);
        core.add_reaction(reaction).await.unwrap();

        let result = core.start_reaction("r_strict").await;
        assert!(
            result.is_err(),
            "Strict policy should fail on config-hash mismatch"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Strict") || msg.contains("manual"),
            "Error should mention Strict policy: {msg}"
        );
    }

    #[tokio::test]
    async fn outbox_catchup_replays_entries_to_reaction() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Inject data so the query has outbox entries.
        inject_events(&core, 3).await;

        // Get the correct config hash for this query.
        let query_arc = core.query_manager.get_query_instance("q1").await.unwrap();
        let config_hash = crate::queries::compute_config_hash(query_arc.get_config());

        // Pre-write a checkpoint with the CORRECT hash but seq=0 (behind the outbox).
        let old_cp = ReactionCheckpoint {
            sequence: 0,
            config_hash,
        };
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), "r_catchup", "q1", &old_cp)
            .await
            .unwrap();

        // Start reaction — should catch up via outbox.
        let reaction = MockReaction::new("r_catchup", vec!["q1".into()])
            .with_snapshot_on_fresh(true)
            .with_policy(ReactionRecoveryPolicy::AutoReset);
        let enqueued = reaction.enqueued.clone();
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r_catchup").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r_catchup",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Verify: outbox entries were replayed to the reaction.
        let results = enqueued.lock().await;
        assert!(
            results.len() >= 3,
            "Expected at least 3 outbox entries replayed, got {}",
            results.len()
        );

        // Verify: checkpoint should have advanced.
        let cp = crate::reactions::checkpoint::read_checkpoint(store.as_ref(), "r_catchup", "q1")
            .await
            .unwrap()
            .expect("Checkpoint should exist");
        assert!(
            cp.sequence > 0,
            "Checkpoint sequence should have advanced from 0, got {}",
            cp.sequence
        );
    }

    // ========================================================================
    // §6 handle_broadcast_gap — direct unit tests
    // ========================================================================

    #[tokio::test]
    async fn broadcast_gap_strict_returns_error() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 10));
        let reaction: Arc<dyn Reaction> = Arc::new(MockReaction::new("r1", vec!["q1".into()]));
        let consumer: Arc<dyn QueryConsumer> = Arc::new(ReactionConsumer(reaction.clone()));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ConsumptionEngine::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::Strict,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
        })
        .await;

        assert!(result.is_err(), "Strict policy should return error on gap");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("Strict"), "Error should mention Strict: {msg}");
        assert!(
            checkpoints.read().await.is_empty(),
            "No checkpoint should be set on Strict error"
        );
    }

    #[tokio::test]
    async fn broadcast_gap_auto_reset_fetches_snapshot_and_bootstraps() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_snapshot_on_fresh(true)
                .with_policy(ReactionRecoveryPolicy::AutoReset),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let consumer: Arc<dyn QueryConsumer> = Arc::new(ReactionConsumer(reaction_trait.clone()));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ConsumptionEngine::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
        })
        .await;

        assert!(
            result.is_ok(),
            "AutoReset should succeed: {:?}",
            result.err()
        );

        // Checkpoint should be set to snapshot sequence.
        let cps = checkpoints.read().await;
        let cp = cps.get("q1").expect("Checkpoint for q1 should exist");
        assert_eq!(
            cp.sequence, 100,
            "Checkpoint should match snapshot sequence"
        );

        // Config hash should be computed from the MockQuery's config.
        let expected_hash = crate::queries::compute_config_hash(query.get_config());
        assert_eq!(cp.config_hash, expected_hash);

        // Bootstrap should have been called exactly once.
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            1,
            "bootstrap() should have been called once"
        );
    }

    #[tokio::test]
    async fn broadcast_gap_auto_skip_gap_jumps_to_current_seq() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 50));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_policy(ReactionRecoveryPolicy::AutoSkipGap),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let consumer: Arc<dyn QueryConsumer> = Arc::new(ReactionConsumer(reaction_trait.clone()));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ConsumptionEngine::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoSkipGap,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
        })
        .await;

        assert!(
            result.is_ok(),
            "AutoSkipGap should succeed: {:?}",
            result.err()
        );

        // Checkpoint should jump to current sequence.
        let cps = checkpoints.read().await;
        let cp = cps.get("q1").expect("Checkpoint for q1 should exist");
        assert_eq!(cp.sequence, 50, "Checkpoint should jump to latest sequence");

        // bootstrap() should NOT be called for AutoSkipGap.
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            0,
            "bootstrap() should not be called for AutoSkipGap"
        );
    }

    // ========================================================================
    // Forwarder sequence-filtering test
    // ========================================================================

    #[tokio::test]
    async fn forwarder_filters_stale_events_after_gate_opens() {
        let store = Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let core = build_core_with_store(store.clone()).await;
        core.start().await.unwrap();

        let mut event_rx = core.subscribe_all_component_events();
        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q1",
            ComponentStatus::Running,
            std::time::Duration::from_secs(10),
        )
        .await;

        // Inject initial data so the query has events at seq > 0.
        inject_events(&core, 5).await;

        // Start reaction — it bootstraps and creates a checkpoint at the current seq.
        let reaction = MockReaction::new("r_filter", vec!["q1".into()])
            .with_snapshot_on_fresh(false)
            .with_policy(ReactionRecoveryPolicy::AutoSkipGap);
        let enqueued = reaction.enqueued.clone();
        core.add_reaction(reaction).await.unwrap();
        core.start_reaction("r_filter").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "r_filter",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Clear any results that may have arrived during startup.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        enqueued.lock().await.clear();

        // Push NEW data after the reaction is running — only these should be forwarded.
        inject_events(&core, 3).await;

        // Verify: only the new events should be enqueued (not the 5 old ones).
        let results = enqueued.lock().await;
        assert_eq!(
            results.len(),
            3,
            "Expected exactly 3 new events, got {} (stale events should be filtered)",
            results.len()
        );
    }

    // ========================================================================
    // §7 Bootstrap failure and concurrent gap recovery tests
    // ========================================================================

    /// MockQuery variant whose `fetch_snapshot` returns an error.
    struct FailingMockQuery {
        config: QueryConfig,
    }

    impl FailingMockQuery {
        fn new() -> Self {
            Self {
                config: QueryConfig {
                    id: "q1".to_string(),
                    query: "MATCH (n) RETURN n".to_string(),
                    query_language: crate::config::schema::QueryLanguage::Cypher,
                    middleware: vec![],
                    sources: vec![],
                    auto_start: true,
                    joins: None,
                    enable_bootstrap: true,
                    bootstrap_buffer_size: 10000,
                    priority_queue_capacity: None,
                    dispatch_buffer_capacity: None,
                    dispatch_mode: None,
                    storage_backend: None,
                    recovery_policy: None,
                    bootstrap_timeout_secs: 300,
                    outbox_capacity: 1000,
                },
            }
        }
    }

    #[async_trait]
    impl crate::queries::Query for FailingMockQuery {
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Running
        }
        fn get_config(&self) -> &QueryConfig {
            &self.config
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn subscribe(&self, _reaction_id: String) -> Result<QuerySubscriptionResponse> {
            Err(anyhow::anyhow!("not supported"))
        }
        async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
            Err(FetchError::NotRunning {
                status: ComponentStatus::Error,
            })
        }
        async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxResponse, FetchError> {
            Ok(OutboxResponse {
                results: vec![],
                latest_sequence: 0,
                config_hash: 0,
            })
        }
    }

    /// Test: AutoReset bootstrap fails because snapshot fetch errors.
    /// The handle_broadcast_gap should propagate the error without leaving
    /// a stale checkpoint in the map.
    #[tokio::test]
    async fn auto_reset_bootstrap_failure_propagates_error() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(FailingMockQuery::new());
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into()])
                .with_snapshot_on_fresh(true)
                .with_policy(ReactionRecoveryPolicy::AutoReset),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let consumer: Arc<dyn QueryConsumer> = Arc::new(ReactionConsumer(reaction_trait.clone()));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ConsumptionEngine::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
        })
        .await;

        // Should fail because fetch_snapshot returns error.
        assert!(
            result.is_err(),
            "AutoReset should fail when snapshot fetch errors"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("failed to fetch snapshot"),
            "Error should mention snapshot failure: {msg}"
        );

        // No checkpoint should be written on failure.
        assert!(
            checkpoints.read().await.is_empty(),
            "No checkpoint should be set when bootstrap fails"
        );

        // bootstrap() should NOT have been called (failed before reaching it).
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            0,
            "bootstrap() should not be called when snapshot fetch fails"
        );
    }

    /// MockReaction variant whose `bootstrap()` always returns an error.
    struct FailingBootstrapReaction {
        id: String,
        queries: Vec<String>,
        policy: ReactionRecoveryPolicy,
        status_handle: ComponentStatusHandle,
        enqueued: Arc<Mutex<Vec<QueryResult>>>,
        bootstrap_count: Arc<AtomicUsize>,
    }

    impl FailingBootstrapReaction {
        fn new(id: &str, queries: Vec<String>) -> Self {
            Self {
                id: id.to_string(),
                queries,
                policy: ReactionRecoveryPolicy::AutoReset,
                status_handle: ComponentStatusHandle::new(id),
                enqueued: Arc::new(Mutex::new(Vec::new())),
                bootstrap_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Reaction for FailingBootstrapReaction {
        fn id(&self) -> &str {
            &self.id
        }
        fn type_name(&self) -> &str {
            "test-failing-bootstrap"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }
        fn auto_start(&self) -> bool {
            false
        }
        async fn initialize(&self, ctx: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(ctx.update_tx.clone()).await;
        }
        async fn start(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, None)
                .await;
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, None)
                .await;
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }
        fn is_durable(&self) -> bool {
            false
        }
        fn needs_snapshot_on_fresh_start(&self) -> bool {
            true
        }
        fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
            self.policy
        }
        async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
            self.enqueued.lock().await.push(result);
            Ok(())
        }
        async fn bootstrap(
            &self,
            _ctx: crate::reactions::bootstrap_context::BootstrapContext,
        ) -> Result<()> {
            self.bootstrap_count.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("Simulated bootstrap failure"))
        }
    }

    /// Test: AutoReset with successful snapshot fetch but bootstrap() hook failure.
    /// The error should propagate and no checkpoint should be persisted.
    #[tokio::test]
    async fn auto_reset_bootstrap_hook_failure_propagates_error() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(FailingBootstrapReaction::new("r1", vec!["q1".into()]));
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let consumer: Arc<dyn QueryConsumer> = Arc::new(ReactionConsumer(reaction_trait.clone()));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        let result = ConsumptionEngine::handle_broadcast_gap(&BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &Arc::new(ReactionMetrics::new()),
        })
        .await;

        // Should fail because bootstrap() returns error.
        assert!(result.is_err(), "Should fail when bootstrap hook errors");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Simulated bootstrap failure"),
            "Error should contain bootstrap failure message: {msg}"
        );

        // No checkpoint should be persisted on bootstrap failure.
        assert!(
            checkpoints.read().await.is_empty(),
            "No checkpoint should be set when bootstrap hook fails"
        );

        // bootstrap() was called once (and failed).
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            1,
            "bootstrap() should have been called once before failing"
        );
    }

    /// Test: Concurrent multi-query gap recovery is serialized by the bootstrap mutex.
    /// Two concurrent AutoReset gap recoveries should not interleave bootstrap calls.
    #[tokio::test]
    async fn concurrent_gap_recovery_serialized_by_mutex() {
        let query: Arc<dyn crate::queries::Query> = Arc::new(MockQuery::new(42, 100));
        let reaction = Arc::new(
            MockReaction::new("r1", vec!["q1".into(), "q2".into()])
                .with_snapshot_on_fresh(true)
                .with_policy(ReactionRecoveryPolicy::AutoReset),
        );
        let reaction_trait: Arc<dyn Reaction> = reaction.clone();
        let consumer: Arc<dyn QueryConsumer> = Arc::new(ReactionConsumer(reaction_trait.clone()));
        let checkpoints = Arc::new(RwLock::new(HashMap::new()));
        let bootstrap_mutex = Arc::new(tokio::sync::Mutex::new(()));

        // Launch two concurrent gap recoveries for different query IDs.
        let metrics_q1 = Arc::new(ReactionMetrics::new());
        let metrics_q2 = Arc::new(ReactionMetrics::new());
        let ctx_q1 = BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q1",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &metrics_q1,
        };
        let ctx_q2 = BroadcastGapContext {
            reaction_id: "r1",
            query_id: "q2",
            consumer: &consumer,
            query: &query,
            policy: ReactionRecoveryPolicy::AutoReset,
            state_store: &None,
            checkpoints: &checkpoints,
            bootstrap_mutex: &bootstrap_mutex,
            metrics: &metrics_q2,
        };
        let (r1, r2) = tokio::join!(
            ConsumptionEngine::handle_broadcast_gap(&ctx_q1),
            ConsumptionEngine::handle_broadcast_gap(&ctx_q2),
        );

        assert!(
            r1.is_ok(),
            "First gap recovery should succeed: {:?}",
            r1.err()
        );
        assert!(
            r2.is_ok(),
            "Second gap recovery should succeed: {:?}",
            r2.err()
        );

        // Both checkpoints should be set.
        let cps = checkpoints.read().await;
        assert!(cps.contains_key("q1"), "Checkpoint for q1 should exist");
        assert!(cps.contains_key("q2"), "Checkpoint for q2 should exist");

        // Bootstrap should have been called exactly twice (once per query, serialized).
        assert_eq!(
            reaction.bootstrap_count.load(Ordering::SeqCst),
            2,
            "bootstrap() should be called exactly twice (serialized by mutex)"
        );
    }
}
