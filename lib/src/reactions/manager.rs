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
use async_trait::async_trait;
use log::{error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::config::{ReactionConfig, ReactionRuntime};
use crate::reactions::common::base::QuerySubscriber;
use crate::server_core::DrasiLib;
use crate::plugin_core::ReactionRegistry;
use crate::utils::*;

/// Trait defining the interface for all reaction implementations.
///
/// # Subscription Model
///
/// Reactions now manage their own subscriptions to queries using the broadcast channel pattern:
/// - Each reaction receives a reference to `DrasiLib` on startup
/// - Reactions access the `QueryManager` via `server_core.query_manager()`
/// - For each query in their configuration, reactions call `query_manager.get_query_instance(query_id)`
/// - They then subscribe directly to each query using `query.subscribe(reaction_id)`
/// - Each subscription provides a broadcast receiver for that query's results
/// - Reactions use a priority queue to process results from multiple queries in timestamp order
///
/// This architecture enables:
/// - Zero-copy result distribution via Arc-wrapped types
/// - Direct query-to-reaction communication without centralized routing
/// - Timestamp-ordered processing across multiple query subscriptions
/// - Independent scaling of query and reaction components
#[async_trait]
pub trait Reaction: Send + Sync {
    /// Start the reaction with access to query subscriptions.
    ///
    /// # Parameters
    /// - `query_subscriber`: Trait object providing access to query instances for subscriptions
    ///
    /// # Implementation Notes
    /// Implementations should:
    /// 1. For each query_id in config.queries, get the query instance via `query_subscriber.get_query_instance()`
    /// 2. Subscribe to each query using `query.subscribe(reaction_id)`
    /// 3. Create a priority queue for timestamp-ordered processing
    /// 4. Spawn task(s) to process results from all subscriptions
    async fn start(&self, query_subscriber: Arc<dyn QuerySubscriber>) -> Result<()>;

    /// Stop the reaction, cleaning up all subscriptions and tasks
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the reaction
    async fn status(&self) -> ComponentStatus;

    /// Get the reaction's configuration
    fn get_config(&self) -> &ReactionConfig;
}

pub struct MockReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
}

impl MockReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
        }
    }
}

#[async_trait]
impl Reaction for MockReaction {
    async fn start(&self, _query_subscriber: Arc<dyn QuerySubscriber>) -> Result<()> {
        log_component_start("Reaction", &self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Reaction started successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Mock reaction - no actual subscription or processing
        // Real reactions would:
        // 1. Access query_manager via server_core.query_manager()
        // 2. Subscribe to each configured query
        // 3. Create priority queue for timestamp-ordered processing
        // 4. Spawn task to process results

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Reaction", &self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Reaction stopped successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.config
    }
}

pub struct ReactionManager {
    reactions: Arc<RwLock<HashMap<String, Arc<dyn Reaction>>>>,
    event_tx: ComponentEventSender,
    registry: ReactionRegistry,
}

impl ReactionManager {
    /// Create a new ReactionManager with default plugin registry
    pub fn new(event_tx: ComponentEventSender) -> Self {
        Self::with_registry(event_tx, ReactionRegistry::default())
    }

    /// Create a new ReactionManager with a custom plugin registry
    pub fn with_registry(event_tx: ComponentEventSender, registry: ReactionRegistry) -> Self {
        Self {
            reactions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            registry,
        }
    }


    /// Add a pre-built reaction instance directly.
    ///
    /// This method allows adding a reaction that was constructed externally,
    /// bypassing the registry-based creation. This is useful for:
    /// - Testing with mock reactions
    /// - Programmatic reaction creation with custom configurations
    /// - Plugin-specific builders that create instances directly
    ///
    /// # Parameters
    /// - `reaction`: The pre-built reaction instance
    ///
    /// # Returns
    /// - `Ok(())` if the reaction was added successfully
    /// - `Err` if a reaction with the same ID already exists
    ///
    /// # Note
    /// The reaction will NOT be auto-started. Call `start_reaction` separately
    /// if you need to start it after adding.
    pub async fn add_reaction_instance(&self, reaction: Arc<dyn Reaction>) -> Result<()> {
        let reaction_id = reaction.get_config().id.clone();

        // Check if reaction with this id already exists
        if self.reactions.read().await.contains_key(&reaction_id) {
            return Err(anyhow::anyhow!(
                "Reaction with id '{}' already exists",
                reaction_id
            ));
        }

        self.reactions.write().await.insert(reaction_id.clone(), reaction);
        info!("Added reaction instance: {}", reaction_id);

        Ok(())
    }

    pub async fn add_reaction(&self, config: ReactionConfig) -> Result<()> {
        self.add_reaction_internal(config).await
    }

    pub async fn add_reaction_without_save(&self, config: ReactionConfig) -> Result<()> {
        self.add_reaction_internal(config).await
    }

    async fn add_reaction_internal(&self, config: ReactionConfig) -> Result<()> {
        // Check if reaction with this id already exists
        if self.reactions.read().await.contains_key(&config.id) {
            return Err(anyhow::anyhow!(
                "Reaction with id '{}' already exists",
                config.id
            ));
        }

        // Create reaction using the plugin registry
        // The plugin registry handles both application and other reaction types
        let reaction = self.registry.create(config.clone(), self.event_tx.clone())?;

        let reaction_id = config.id.clone();
        let should_auto_start = config.auto_start;

        self.reactions
            .write()
            .await
            .insert(config.id.clone(), reaction);
        info!("Added reaction: {}", config.id);

        // Note: Auto-start is handled by the caller (server.create_reaction)
        // which has access to the subscription router for subscriptions
        if should_auto_start {
            info!(
                "Reaction '{}' is configured for auto-start (will be started by caller)",
                reaction_id
            );
        }

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
            let config = reaction.get_config();
            let runtime = ReactionRuntime {
                id: config.id.clone(),
                reaction_type: config.reaction_type().to_string(),
                status: status.clone(),
                error_message: match &status {
                    ComponentStatus::Error => Some("Reaction error occurred".to_string()),
                    _ => None,
                },
                queries: config.queries.clone(),
                properties: config.get_properties(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Reaction not found: {}", id))
        }
    }

    /// Update a reaction configuration.
    ///
    /// # Parameters
    /// - `id`: The reaction ID to update
    /// - `config`: The new configuration
    /// - `query_subscriber`: Optional query subscriber. Required if the reaction is currently running
    ///   and needs to be restarted after the update.
    pub async fn update_reaction(
        &self,
        id: String,
        config: ReactionConfig,
        query_subscriber: Option<Arc<dyn QuerySubscriber>>,
    ) -> Result<()> {
        // Check status outside lock
        let reaction = {
            let reactions = self.reactions.read().await;
            reactions.get(&id).cloned()
        }
        .ok_or_else(|| anyhow::anyhow!("Reaction not found: {}", id))?;

        let status = reaction.status().await;
        let was_running = matches!(status, ComponentStatus::Running | ComponentStatus::Starting);

        // If running, we need to stop it first
        if was_running {
            self.stop_reaction(id.clone()).await?;

            // Verify stopped state
            let reaction_check = {
                let reactions = self.reactions.read().await;
                reactions.get(&id).cloned()
            };

            if let Some(reaction) = reaction_check {
                let status = reaction.status().await;
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
            }
        } else {
            is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
        }

        // For now, update means remove and re-add
        self.delete_reaction(id.clone()).await?;
        self.add_reaction(config).await?;

        // If it was running, restart it
        if was_running {
            if let Some(subscriber) = query_subscriber {
                self.start_reaction(id, subscriber).await?;
            } else {
                return Err(anyhow::anyhow!(
                    "Cannot restart reaction '{}' after update: query_subscriber parameter required",
                    id
                ));
            }
        }

        Ok(())
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

    pub async fn get_reaction_config(&self, name: &str) -> Option<ReactionConfig> {
        let reactions = self.reactions.read().await;
        reactions.get(name).map(|r| r.get_config().clone())
    }

    /// Start all reactions marked for auto-start.
    ///
    /// # Parameters
    /// - `server_core`: Arc reference to DrasiLib for query subscriptions
    ///
    /// Reactions will manage their own subscriptions to queries using the broadcast channel pattern.
    pub async fn start_all(&self, server_core: Arc<DrasiLib>) -> Result<()> {
        let reactions = self.reactions.read().await;
        let mut failed_reactions = Vec::new();

        for (id, reaction) in reactions.iter() {
            let config = reaction.get_config();
            if config.auto_start {
                info!("Auto-starting reaction: {}", id);
                let subscriber: Arc<dyn QuerySubscriber> = server_core.clone();
                if let Err(e) = reaction.start(subscriber).await {
                    error!("Failed to start reaction {}: {}", id, e);
                    failed_reactions.push((id.clone(), e.to_string()));
                    // Continue starting other reactions instead of returning early
                }
            } else {
                info!(
                    "Reaction '{}' has auto_start=false, skipping automatic startup",
                    id
                );
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
        let reactions = self.reactions.read().await;
        for reaction in reactions.values() {
            if let Err(e) = reaction.stop().await {
                log_component_error("Reaction", &reaction.get_config().id, &e.to_string());
            }
        }
        Ok(())
    }
}
