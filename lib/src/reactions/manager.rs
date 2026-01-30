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
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::config::ReactionRuntime;
use crate::context::ReactionRuntimeContext;
use crate::managers::{
    is_operation_valid, log_component_error, ComponentEventHistory, ComponentLogRegistry, Operation,
};
use crate::reactions::{QueryProvider, Reaction};
use crate::state_store::StateStoreProvider;

pub struct ReactionManager {
    reactions: Arc<RwLock<HashMap<String, Arc<dyn Reaction>>>>,
    event_tx: ComponentEventSender,
    /// Query provider for reactions to access queries (injected after DrasiLib is constructed)
    query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
    /// State store provider for reactions to persist state
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Event history for tracking component lifecycle events
    event_history: Arc<RwLock<ComponentEventHistory>>,
    /// Log registry for component log streaming
    log_registry: Arc<ComponentLogRegistry>,
}

impl ReactionManager {
    /// Create a new ReactionManager
    pub fn new(event_tx: ComponentEventSender, log_registry: Arc<ComponentLogRegistry>) -> Self {
        Self {
            reactions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            query_provider: Arc::new(RwLock::new(None)),
            state_store: Arc::new(RwLock::new(None)),
            event_history: Arc::new(RwLock::new(ComponentEventHistory::new())),
            log_registry,
        }
    }

    /// Inject the query provider (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to look up queries when they start.
    pub async fn inject_query_provider(&self, qp: Arc<dyn QueryProvider>) {
        *self.query_provider.write().await = Some(qp);
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

        // Query provider must be available for reactions
        let query_provider = self.query_provider.read().await.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "QueryProvider not injected - was ReactionManager initialized properly?"
            )
        })?;

        // Create a component logger for this reaction
        let logger = self
            .log_registry
            .create_logger(&reaction_id, ComponentType::Reaction);

        // Construct runtime context for this reaction
        let context = ReactionRuntimeContext::with_logger(
            &reaction_id,
            self.event_tx.clone(),
            self.state_store.read().await.clone(),
            query_provider,
            logger,
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

    pub async fn delete_reaction(&self, id: String) -> Result<()> {
        // First check if the reaction exists
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            let status = reaction.status().await;

            // If the reaction is running, stop it first
            if matches!(status, ComponentStatus::Running) {
                info!("Stopping reaction '{id}' before deletion");
                reaction.stop().await?;

                // Wait a bit to ensure the reaction has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped
                let new_status = reaction.status().await;
                if !matches!(new_status, ComponentStatus::Stopped) {
                    return Err(anyhow::anyhow!(
                        "Failed to stop reaction '{id}' before deletion"
                    ));
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Now remove the reaction
            self.reactions.write().await.remove(&id);
            // Clean up event history for this reaction
            self.event_history.write().await.remove_component(&id);
            info!("Deleted reaction: {id}");

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
        let reactions: Vec<Arc<dyn Reaction>> = {
            let reactions = self.reactions.read().await;
            reactions.values().cloned().collect()
        };

        let mut failed_reactions = Vec::new();

        for reaction in reactions {
            // Only start reactions with auto_start enabled
            if !reaction.auto_start() {
                info!("Skipping reaction '{}' (auto_start=false)", reaction.id());
                continue;
            }

            let reaction_id = reaction.id().to_string();
            info!("Starting reaction: {reaction_id}");
            if let Err(e) = reaction.start().await {
                error!("Failed to start reaction {reaction_id}: {e}");
                failed_reactions.push((reaction_id, e.to_string()));
                // Continue starting other reactions instead of returning early
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

        Some(self.log_registry.subscribe(id).await)
    }
}
