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
use crate::managers::{is_operation_valid, log_component_error, Operation};
use crate::plugin_core::{QuerySubscriber, Reaction};

pub struct ReactionManager {
    reactions: Arc<RwLock<HashMap<String, Arc<dyn Reaction>>>>,
    event_tx: ComponentEventSender,
    /// Query subscriber for reactions to access queries (injected after DrasiLib is constructed)
    query_subscriber: Arc<RwLock<Option<Arc<dyn QuerySubscriber>>>>,
}

impl ReactionManager {
    /// Create a new ReactionManager
    pub fn new(event_tx: ComponentEventSender) -> Self {
        Self {
            reactions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            query_subscriber: Arc::new(RwLock::new(None)),
        }
    }

    /// Inject the query subscriber (called after DrasiLib is fully constructed)
    ///
    /// This allows reactions to look up queries when they start.
    pub async fn inject_query_subscriber(&self, qs: Arc<dyn QuerySubscriber>) {
        *self.query_subscriber.write().await = Some(qs);
    }

    /// Add a reaction instance, taking ownership and wrapping it in an Arc internally.
    ///
    /// # Parameters
    /// - `reaction`: The reaction instance to add (ownership is transferred)
    ///
    /// # Returns
    /// - `Ok(())` if the reaction was added successfully
    /// - `Err` if a reaction with the same ID already exists
    ///
    /// # Note
    /// The reaction will NOT be auto-started. Call `start_reaction` separately
    /// if you need to start it after adding.
    ///
    /// Dependencies (event channel and query subscriber) are automatically injected
    /// into the reaction before it is stored.
    ///
    /// # Example
    /// ```ignore
    /// let reaction = MyReaction::new("my-reaction", vec!["query1".into()]);
    /// manager.add_reaction(reaction).await?;  // Ownership transferred
    /// ```
    pub async fn add_reaction(&self, reaction: impl Reaction + 'static) -> Result<()> {
        let reaction: Arc<dyn Reaction> = Arc::new(reaction);
        let reaction_id = reaction.id().to_string();

        // Inject dependencies before storing
        reaction.inject_event_tx(self.event_tx.clone()).await;

        // Inject query subscriber if available
        if let Some(qs) = self.query_subscriber.read().await.as_ref() {
            reaction.inject_query_subscriber(qs.clone()).await;
        }

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
            let runtime = ReactionRuntime {
                id: reaction.id().to_string(),
                reaction_type: reaction.type_name().to_string(),
                status: status.clone(),
                error_message: match &status {
                    ComponentStatus::Error => Some("Reaction error occurred".to_string()),
                    _ => None,
                },
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
}
