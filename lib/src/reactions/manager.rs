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
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::channels::*;
use crate::component_graph::{ComponentGraph, ComponentKind, ComponentUpdateSender};
use crate::config::ReactionRuntime;
use crate::context::ReactionRuntimeContext;
use crate::identity::IdentityProvider;
use crate::managers::{log_component_error, ComponentLogKey, ComponentLogRegistry};
use crate::queries::Query;
use crate::reactions::{QueryProvider, Reaction};
use crate::state_store::StateStoreProvider;

pub struct ReactionManager {
    instance_id: String,
    /// Query provider for reactions to access queries (injected after DrasiLib is constructed)
    query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
    /// State store provider for reactions to persist state
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Identity provider for credential injection
    identity_provider: Arc<RwLock<Option<Arc<dyn IdentityProvider>>>>,
    /// Event history for tracking component lifecycle events
    event_history: Arc<RwLock<ComponentEventHistory>>,
    /// Log registry for component log streaming
    log_registry: Arc<ComponentLogRegistry>,
    /// Handles to subscription forwarder tasks per reaction
    subscription_tasks: Arc<RwLock<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>>,
    /// Shared component graph — the single source of truth for component metadata,
    /// state, relationships, runtime instances, AND event history.
    graph: Arc<RwLock<ComponentGraph>>,
    /// Channel sender for routing status updates through the graph update loop.
    /// Managers send transitional states (Starting, Stopping, Reconfiguring) here;
    /// the loop applies them to the graph and records events automatically.
    update_tx: ComponentUpdateSender,
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
        Self {
            instance_id: instance_id.into(),
            query_provider: Arc::new(RwLock::new(None)),
            state_store: Arc::new(RwLock::new(None)),
            identity_provider: Arc::new(RwLock::new(None)),
            event_history: Arc::new(RwLock::new(ComponentEventHistory::new())),
            log_registry,
            subscription_tasks: Arc::new(RwLock::new(HashMap::new())),
            graph,
            update_tx,
        }
    }

    /// Inject the query provider (called after DrasiLib is fully constructed)
    ///
    /// This allows the ReactionManager to provide query access to reactions.
    pub async fn inject_query_provider(&self, qp: Arc<dyn QueryProvider>) {
        *self.query_provider.write().await = Some(qp);
    }

    /// Inject the state store provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to access the state store when they are added.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
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

        // Construct runtime context for this reaction
        let mut context = ReactionRuntimeContext::new(
            &self.instance_id,
            &reaction_id,
            self.state_store.read().await.clone(),
            self.update_tx.clone(),
            None,
        );
        context.identity_provider = self.identity_provider.read().await.clone();

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

        crate::managers::lifecycle_helpers::start_component(
            &self.graph,
            &id,
            "reaction",
            &reaction,
        )
        .await?;

        if let Err(e) = self.subscribe_reaction_to_queries(&id, reaction).await {
            // Revert to Error since the reaction can't receive data without subscriptions
            let mut graph = self.graph.write().await;
            let _ = graph.validate_and_transition(
                &id,
                ComponentStatus::Error,
                Some(format!("Subscription setup failed: {e}")),
            );
            return Err(e);
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

        // Abort subscription forwarder tasks before stopping
        self.abort_subscription_tasks(&id).await;

        crate::managers::lifecycle_helpers::stop_component(&self.graph, &id, "reaction", &reaction)
            .await
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
        let sub_tasks = self.subscription_tasks.clone();
        crate::managers::lifecycle_helpers::teardown_component::<Arc<dyn Reaction>, _, _>(
            &self.graph,
            &id,
            "reaction",
            ComponentType::Reaction,
            &self.instance_id,
            &self.log_registry,
            cleanup,
            || Self::abort_subscription_tasks_static(&sub_tasks, &id_clone),
        )
        .await?;

        // Also abort any remaining subscription tasks after teardown
        self.abort_subscription_tasks(&id).await;
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
                    let context = ReactionRuntimeContext::new(
                        instance_id,
                        &id,
                        state_store.read().await.clone(),
                        update_tx.clone(),
                        None,
                    );
                    new_reaction.initialize(context).await;

                    let mut g = graph.write().await;
                    if !g.has_runtime(&id) {
                        return Err(anyhow::anyhow!(
                            "Reaction '{id}' was concurrently deleted during reconfiguration"
                        ));
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }

                let status = old_reaction.status().await;
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Initialize the new reaction with runtime context
            let new_reaction: Arc<dyn Reaction> = Arc::new(new_reaction);
            let mut context = ReactionRuntimeContext::new(
                &self.instance_id,
                &id,
                self.event_tx.clone(),
                self.state_store.read().await.clone(),
            );
            context.identity_provider = self.identity_provider.read().await.clone();
            new_reaction.initialize(context).await;

            // Replace in the reactions map only if the entry still exists
            // (guards against concurrent deletion)
            {
                let mut reactions = self.reactions.write().await;
                if !reactions.contains_key(&id) {
                    return Err(anyhow::anyhow!(
                        "Reaction '{id}' was concurrently deleted during reconfiguration"
                    ));
                }
                reactions.insert(id.clone(), new_reaction);
            }

            info!("Reconfigured reaction '{id}'");

            // Restart if it was running before
            if was_running {
                self.start_reaction(id).await?;
            }

            Ok(())
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
        let mut graph = self.graph.write().await;
        if !graph.has_runtime(id) {
            return None;
        }
        Some(graph.subscribe_events(id))
    }

    /// Subscribe a reaction to its configured queries and spawn forwarder tasks.
    ///
    /// The host manages query subscriptions on behalf of the reaction.
    /// For each query the reaction is interested in, the manager:
    /// 1. Gets the query instance from the QueryProvider
    /// 2. Subscribes to the query's result stream
    /// 3. Spawns a forwarder task that calls `reaction.enqueue_query_result()`
    ///
    /// # Locking invariant
    ///
    /// The `query_provider` RwLock is held only long enough to clone the inner
    /// `Arc<dyn QueryProvider>`. All subsequent calls (`get_query_instance`, `subscribe`)
    /// operate on the cloned Arc, so no locks are held while calling into external code.
    async fn subscribe_reaction_to_queries(
        &self,
        reaction_id: &str,
        reaction: Arc<dyn Reaction>,
    ) -> Result<()> {
        let query_ids = reaction.query_ids();
        if query_ids.is_empty() {
            return Ok(());
        }

        // Clone the Arc and release the RwLock guard immediately.
        let query_provider = self.query_provider.read().await.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "QueryProvider not injected - was ReactionManager initialized properly?"
            )
        })?;

        let instance_id = self.instance_id.clone();
        let mut tasks = Vec::new();

        for query_id in &query_ids {
            let query = query_provider.get_query_instance(query_id).await?;

            let subscription = query.subscribe(reaction_id.to_string()).await?;
            let mut receiver = subscription.receiver;

            let reaction = reaction.clone();
            let query_id_clone = query_id.clone();
            let reaction_id_owned = reaction_id.to_string();

            let query_config = query.get_config();
            let dispatch_mode = query_config
                .dispatch_mode
                .unwrap_or(crate::channels::DispatchMode::Channel);
            let _use_blocking_enqueue =
                matches!(dispatch_mode, crate::channels::DispatchMode::Channel);

            let span = tracing::info_span!(
                "reaction_forwarder",
                instance_id = %instance_id,
                component_id = %reaction_id_owned,
                component_type = "reaction"
            );

            let forwarder_task = tokio::spawn(
                async move {
                    log::debug!(
                        "[{reaction_id_owned}] Started result forwarder for query '{query_id_clone}'"
                    );

                    loop {
                        match receiver.recv().await {
                            Ok(query_result) => {
                                // Unwrap Arc or clone if shared
                                let result = Arc::try_unwrap(query_result)
                                    .unwrap_or_else(|arc| (*arc).clone());
                                if let Err(e) = reaction.enqueue_query_result(result).await {
                                    log::error!(
                                        "[{reaction_id_owned}] Failed to enqueue result from query '{query_id_clone}': {e}"
                                    );
                                }
                            }
                            Err(e) => {
                                let error_str = e.to_string();
                                if error_str.contains("lagged") {
                                    log::warn!(
                                        "[{reaction_id_owned}] Receiver lagged for query '{query_id_clone}': {error_str}"
                                    );
                                    continue;
                                } else {
                                    log::info!(
                                        "[{reaction_id_owned}] Receiver closed for query '{query_id_clone}': {error_str}"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
                .instrument(span),
            );

            tasks.push(forwarder_task);
        }

        // Store the forwarder task handles
        self.subscription_tasks
            .write()
            .await
            .insert(reaction_id.to_string(), tasks);

        Ok(())
    }

    /// Abort all subscription forwarder tasks for a reaction.
    async fn abort_subscription_tasks(&self, reaction_id: &str) {
        Self::abort_subscription_tasks_static(&self.subscription_tasks, reaction_id).await;
    }

    async fn abort_subscription_tasks_static(
        tasks: &Arc<RwLock<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>>,
        reaction_id: &str,
    ) {
        if let Some(tasks) = tasks.write().await.remove(reaction_id) {
            for task in tasks {
                task.abort();
            }
        }
    }
}
