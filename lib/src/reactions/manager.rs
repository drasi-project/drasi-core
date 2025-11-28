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
use crate::plugin_core::{QuerySubscriber, Reaction};
use crate::server_core::DrasiLib;
use crate::utils::*;

pub struct ReactionManager {
    reactions: Arc<RwLock<HashMap<String, Arc<dyn Reaction>>>>,
    #[allow(dead_code)]
    event_tx: ComponentEventSender,
}

impl ReactionManager {
    /// Create a new ReactionManager
    pub fn new(event_tx: ComponentEventSender) -> Self {
        Self {
            reactions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
        }
    }

    /// Add a reaction instance.
    ///
    /// # Parameters
    /// - `reaction`: The reaction instance to add
    ///
    /// # Returns
    /// - `Ok(())` if the reaction was added successfully
    /// - `Err` if a reaction with the same ID already exists
    ///
    /// # Note
    /// The reaction will NOT be auto-started. Call `start_reaction` separately
    /// if you need to start it after adding.
    ///
    /// The event channel is automatically injected into the reaction before it is stored.
    pub async fn add_reaction(&self, reaction: Arc<dyn Reaction>) -> Result<()> {
        let reaction_id = reaction.id().to_string();

        // Check if reaction with this id already exists
        if self.reactions.read().await.contains_key(&reaction_id) {
            return Err(anyhow::anyhow!(
                "Reaction with id '{}' already exists",
                reaction_id
            ));
        }

        // Inject the event channel before storing
        reaction.inject_event_tx(self.event_tx.clone()).await;

        self.reactions
            .write()
            .await
            .insert(reaction_id.clone(), reaction);
        info!("Added reaction: {}", reaction_id);

        Ok(())
    }

    /// Start a reaction with access to the server core for query subscriptions.
    ///
    /// # Parameters
    /// - `id`: The reaction ID to start
    /// - `query_subscriber`: Trait object providing access to query instances for subscriptions
    pub async fn start_reaction(
        &self,
        id: String,
        query_subscriber: Arc<dyn QuerySubscriber>,
    ) -> Result<()> {
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        };

        if let Some(reaction) = reaction {
            let status = reaction.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            reaction.start(query_subscriber).await?;
        } else {
            return Err(anyhow::anyhow!("Reaction not found: {}", id));
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
            return Err(anyhow::anyhow!("Reaction not found: {}", id));
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
            Err(anyhow::anyhow!("Reaction not found: {}", id))
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
            Err(anyhow::anyhow!("Reaction not found: {}", id))
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
                info!("Stopping reaction '{}' before deletion", id);
                reaction.stop().await?;

                // Wait a bit to ensure the reaction has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped
                let new_status = reaction.status().await;
                if !matches!(new_status, ComponentStatus::Stopped) {
                    return Err(anyhow::anyhow!(
                        "Failed to stop reaction '{}' before deletion",
                        id
                    ));
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Now remove the reaction
            self.reactions.write().await.remove(&id);
            info!("Deleted reaction: {}", id);

            Ok(())
        } else {
            Err(anyhow::anyhow!("Reaction not found: {}", id))
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

    /// Start all reactions.
    ///
    /// # Parameters
    /// - `server_core`: Arc reference to DrasiLib for query subscriptions
    ///
    /// Reactions will manage their own subscriptions to queries using the broadcast channel pattern.
    pub async fn start_all(&self, server_core: Arc<DrasiLib>) -> Result<()> {
        let reactions: Vec<Arc<dyn Reaction>> = {
            let reactions = self.reactions.read().await;
            reactions.values().cloned().collect()
        };

        let mut failed_reactions = Vec::new();

        for reaction in reactions {
            let reaction_id = reaction.id().to_string();
            info!("Starting reaction: {}", reaction_id);
            let subscriber: Arc<dyn QuerySubscriber> = server_core.clone();
            if let Err(e) = reaction.start(subscriber).await {
                error!("Failed to start reaction {}: {}", reaction_id, e);
                failed_reactions.push((reaction_id, e.to_string()));
                // Continue starting other reactions instead of returning early
            }
        }

        // Return error only if any reactions failed to start
        if !failed_reactions.is_empty() {
            let error_msg = failed_reactions
                .iter()
                .map(|(id, err)| format!("{}: {}", id, err))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "Failed to start some reactions: {}",
                error_msg
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
