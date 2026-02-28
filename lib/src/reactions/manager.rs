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
use crate::config::ReactionRuntime;
use crate::context::ReactionRuntimeContext;
use crate::managers::{
    is_operation_valid, log_component_error, ComponentEventHistory, ComponentLogKey,
    ComponentLogRegistry, Operation,
};
use crate::queries::Query;
use crate::reactions::Reaction;
use crate::state_store::StateStoreProvider;

pub struct ReactionManager {
    instance_id: String,
    reactions: Arc<RwLock<HashMap<String, Arc<dyn Reaction>>>>,
    event_tx: ComponentEventSender,
    /// Query manager for subscribing reactions to query results
    query_manager: Arc<RwLock<Option<Arc<crate::queries::QueryManager>>>>,
    /// State store provider for reactions to persist state
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Event history for tracking component lifecycle events
    event_history: Arc<RwLock<ComponentEventHistory>>,
    /// Log registry for component log streaming
    log_registry: Arc<ComponentLogRegistry>,
    /// Handles to subscription forwarder tasks per reaction
    subscription_tasks: Arc<RwLock<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>>,
}

impl ReactionManager {
    /// Create a new ReactionManager
    pub fn new(
        instance_id: impl Into<String>,
        event_tx: ComponentEventSender,
        log_registry: Arc<ComponentLogRegistry>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            reactions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            query_manager: Arc::new(RwLock::new(None)),
            state_store: Arc::new(RwLock::new(None)),
            event_history: Arc::new(RwLock::new(ComponentEventHistory::new())),
            log_registry,
            subscription_tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Inject the query manager (called after DrasiLib is fully constructed)
    ///
    /// This allows the ReactionManager to subscribe reactions to query results.
    pub async fn inject_query_manager(&self, qm: Arc<crate::queries::QueryManager>) {
        *self.query_manager.write().await = Some(qm);
    }

    /// Inject the state store provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to access the state store when they are added.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }

    /// Add a reaction instance, taking ownership and wrapping it in an Arc internally.
    ///
    /// # Parameters
    /// - `reaction`: The reaction instance to add (ownership is transferred)
    ///
    /// # Returns
    /// - `Ok(())` if the reaction was added successfully
    /// - `Err` if a reaction with the same ID already exists, or if query subscriber not injected
    ///
    /// # Note
    /// The reaction will NOT be auto-started. Call `start_reaction` separately
    /// if you need to start it after adding.
    ///
    /// The reaction is automatically initialized with the runtime context before
    /// it is stored.
    ///
    /// # Example
    /// ```ignore
    /// let reaction = MyReaction::new("my-reaction", vec!["query1".into()]);
    /// manager.add_reaction(reaction).await?;  // Ownership transferred
    /// ```
    pub async fn add_reaction(&self, reaction: impl Reaction + 'static) -> Result<()> {
        let reaction: Arc<dyn Reaction> = Arc::new(reaction);
        let reaction_id = reaction.id().to_string();

        // Construct runtime context for this reaction
        let context = ReactionRuntimeContext::new(
            &self.instance_id,
            &reaction_id,
            self.event_tx.clone(),
            self.state_store.read().await.clone(),
        );

        // Initialize the reaction with its runtime context
        reaction.initialize(context).await;

        // Use a single write lock to atomically check and insert
        // This eliminates the TOCTOU race condition from separate read-then-write
        {
            let mut reactions = self.reactions.write().await;
            if reactions.contains_key(&reaction_id) {
                return Err(anyhow::anyhow!(
                    "Reaction with id '{reaction_id}' already exists"
                ));
            }
            reactions.insert(reaction_id.clone(), reaction);
        }

        info!("Added reaction: {reaction_id}");
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
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            let status = reaction.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            reaction.start().await?;

            // Subscribe to queries on behalf of the reaction and forward results
            self.subscribe_reaction_to_queries(&id, reaction).await?;
        } else {
            return Err(anyhow::anyhow!("Reaction not found: {id}"));
        }

        Ok(())
    }

    pub async fn stop_reaction(&self, id: String) -> Result<()> {
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            let status = reaction.status().await;
            is_operation_valid(&status, &Operation::Stop).map_err(|e| anyhow::anyhow!(e))?;

            // Abort subscription forwarder tasks
            self.abort_subscription_tasks(&id).await;

            reaction.stop().await?;
        } else {
            return Err(anyhow::anyhow!("Reaction not found: {id}"));
        }

        Ok(())
    }

    pub async fn get_reaction_status(&self, id: String) -> Result<ComponentStatus> {
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            Ok(reaction.status().await)
        } else {
            Err(anyhow::anyhow!("Reaction not found: {id}"))
        }
    }

    pub async fn get_reaction(&self, id: String) -> Result<ReactionRuntime> {
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            let status = reaction.status().await;
            let error_message = match &status {
                ComponentStatus::Error => self.event_history.read().await.get_last_error(&id),
                _ => None,
            };
            let runtime = ReactionRuntime {
                id: reaction.id().to_string(),
                reaction_type: reaction.type_name().to_string(),
                status: status.clone(),
                error_message,
                queries: reaction.query_ids(),
                properties: reaction.properties(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Reaction not found: {id}"))
        }
    }

    pub async fn delete_reaction(&self, id: String, cleanup: bool) -> Result<()> {
        // First check if the reaction exists
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            let status = reaction.status().await;

            // If the reaction is running or starting, stop it first
            if matches!(status, ComponentStatus::Running | ComponentStatus::Starting) {
                info!("Stopping reaction '{id}' before deletion");
                // Abort subscription forwarder tasks
                self.abort_subscription_tasks(&id).await;
                if let Err(e) = reaction.stop().await {
                    warn!("Failed to stop reaction '{id}' during deletion (may already be stopped): {e}");
                }

                // Wait a bit to ensure the reaction has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped - accept Stopped or Error
                let new_status = reaction.status().await;
                if !matches!(
                    new_status,
                    ComponentStatus::Stopped | ComponentStatus::Error
                ) {
                    warn!("Reaction '{id}' in unexpected state {new_status:?} after stop, proceeding with deletion");
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Call deprovision if cleanup is requested
            if cleanup {
                if let Err(e) = reaction.deprovision().await {
                    tracing::warn!("Deprovision failed for reaction '{id}': {e}");
                }
            }

            // Now remove the reaction
            self.reactions.write().await.remove(&id);
            self.abort_subscription_tasks(&id).await;
            // Clean up event history for this reaction
            self.event_history.write().await.remove_component(&id);
            // Clean up log registry for this reaction
            let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Reaction, &id);
            self.log_registry.remove_component_by_key(&log_key).await;
            info!("Deleted reaction: {id}");

            Ok(())
        } else {
            Err(anyhow::anyhow!("Reaction not found: {id}"))
        }
    }

    /// Update a reaction by replacing it with a new instance.
    ///
    /// Flow: validate exists → validate status → emit Reconfiguring event →
    /// stop if running/starting → wait for stopped → initialize new →
    /// replace (if still exists) → restart if was running.
    /// Log and event history are preserved.
    pub async fn update_reaction(
        &self,
        id: String,
        new_reaction: impl Reaction + 'static,
    ) -> Result<()> {
        let old_reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
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

            let status = old_reaction.status().await;
            let was_running =
                matches!(status, ComponentStatus::Running | ComponentStatus::Starting);

            // Validate that update is allowed in the current state
            if was_running {
                // Running/Starting components will be stopped, then validated
            } else {
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Emit Reconfiguring event
            let _ = self
                .event_tx
                .send(ComponentEvent {
                    component_id: id.clone(),
                    component_type: ComponentType::Reaction,
                    status: ComponentStatus::Reconfiguring,
                    timestamp: chrono::Utc::now(),
                    message: Some("Reconfiguring reaction".to_string()),
                })
                .await;

            // If running or starting, stop first then validate
            if was_running {
                info!("Stopping reaction '{id}' for reconfiguration");
                self.abort_subscription_tasks(&id).await;
                old_reaction.stop().await?;

                // Poll for Stopped status with timeout
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
                loop {
                    let current = old_reaction.status().await;
                    if matches!(current, ComponentStatus::Stopped | ComponentStatus::Error) {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(anyhow::anyhow!(
                            "Timed out waiting for reaction '{id}' to stop"
                        ));
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }

                let status = old_reaction.status().await;
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Initialize the new reaction with runtime context
            let new_reaction: Arc<dyn Reaction> = Arc::new(new_reaction);
            let context = ReactionRuntimeContext::new(
                &self.instance_id,
                &id,
                self.event_tx.clone(),
                self.state_store.read().await.clone(),
            );
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
            Err(anyhow::anyhow!("Reaction not found: {id}"))
        }
    }

    pub async fn list_reactions(&self) -> Vec<(String, ComponentStatus)> {
        let reactions_snapshot = {
            let reactions = self.reactions.read().await;
            reactions
                .iter()
                .map(|(id, r)| (id.clone(), r.clone()))
                .collect::<Vec<_>>()
        };

        let mut result = Vec::new();
        for (id, reaction) in reactions_snapshot {
            let status = reaction.status().await;
            result.push((id, status));
        }

        result
    }

    /// Start all reactions that have `auto_start` enabled.
    ///
    /// Reactions must have been added via `add_reaction()` first, which injects
    /// the necessary dependencies (event channel and query subscriber).
    ///
    /// Only reactions with `auto_start() == true` will be started.
    pub async fn start_all(&self) -> Result<()> {
        let reaction_ids: Vec<String> = {
            let reactions = self.reactions.read().await;
            reactions.keys().cloned().collect()
        };

        let mut failed_reactions = Vec::new();

        for reaction_id in reaction_ids {
            // Check auto_start
            let reaction = {
                let reactions = self.reactions.read().await;
                reactions.get(&reaction_id).cloned()
            };

            if let Some(reaction) = reaction {
                if !reaction.auto_start() {
                    info!("Skipping reaction '{}' (auto_start=false)", reaction_id);
                    continue;
                }

                info!("Starting reaction: {reaction_id}");
                if let Err(e) = self.start_reaction(reaction_id.clone()).await {
                    error!("Failed to start reaction {reaction_id}: {e}");
                    failed_reactions.push((reaction_id, e.to_string()));
                }
            }
        }

        // Return error only if any reactions failed to start
        if !failed_reactions.is_empty() {
            let error_msg = failed_reactions
                .iter()
                .map(|(id, err)| format!("{id}: {err}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "Failed to start some reactions: {error_msg}"
            ))
        } else {
            Ok(())
        }
    }

    pub async fn stop_all(&self) -> Result<()> {
        let reactions: Vec<Arc<dyn Reaction>> = {
            let reactions = self.reactions.read().await;
            reactions.values().cloned().collect()
        };

        for reaction in reactions {
            if let Err(e) = reaction.stop().await {
                log_component_error("Reaction", reaction.id(), &e.to_string());
            }
        }
        Ok(())
    }

    /// Record a component event in the history.
    ///
    /// This should be called by the event processing loop to track component
    /// lifecycle events for later querying.
    pub async fn record_event(&self, event: ComponentEvent) {
        self.event_history.write().await.record_event(event);
    }

    /// Get events for a specific reaction.
    ///
    /// Returns events in chronological order (oldest first).
    pub async fn get_reaction_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.event_history.read().await.get_events(id)
    }

    /// Get all events across all reactions.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.event_history.read().await.get_all_events()
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
        // Verify the reaction exists
        {
            let reactions = self.reactions.read().await;
            if !reactions.contains_key(id) {
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
        // Verify the reaction exists
        {
            let reactions = self.reactions.read().await;
            if !reactions.contains_key(id) {
                return None;
            }
        }

        Some(self.event_history.write().await.subscribe(id))
    }

    /// Get a shared reference to the event history.
    pub fn event_history(&self) -> Arc<RwLock<ComponentEventHistory>> {
        Arc::clone(&self.event_history)
    }

    /// Subscribe a reaction to its configured queries and spawn forwarder tasks.
    ///
    /// This is called by the host after `reaction.start()` succeeds.
    /// For each query the reaction is interested in, the manager:
    /// 1. Gets the query instance from the QueryManager
    /// 2. Subscribes to the query's result stream
    /// 3. Spawns a forwarder task that calls `reaction.enqueue_query_result()`
    async fn subscribe_reaction_to_queries(
        &self,
        reaction_id: &str,
        reaction: Arc<dyn Reaction>,
    ) -> Result<()> {
        let query_ids = reaction.query_ids();
        if query_ids.is_empty() {
            return Ok(());
        }

        let query_manager = self.query_manager.read().await.clone().ok_or_else(|| {
            anyhow::anyhow!("QueryManager not injected - was ReactionManager initialized properly?")
        })?;

        let instance_id = self.instance_id.clone();
        let mut tasks = Vec::new();

        for query_id in &query_ids {
            let query = query_manager
                .get_query_instance(query_id)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;

            let subscription = query
                .subscribe(reaction_id.to_string())
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
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
                                reaction.enqueue_query_result(result).await;
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
        if let Some(tasks) = self.subscription_tasks.write().await.remove(reaction_id) {
            for task in tasks {
                task.abort();
            }
        }
    }
}
