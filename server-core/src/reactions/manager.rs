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
use crate::routers::SubscriptionRouter;
use crate::utils::*;

use super::{
    AdaptiveGrpcReaction, AdaptiveHttpReaction, ApplicationReaction, ApplicationReactionHandle,
    GrpcReaction, HttpReaction, LogReaction, PlatformReaction, ProfilerReaction, SseReaction,
};

#[async_trait]
pub trait Reaction: Send + Sync {
    async fn start(&self, result_rx: QueryResultReceiver) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
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
    async fn start(&self, mut result_rx: QueryResultReceiver) -> Result<()> {
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

        // Start processing query results
        let query_filter = self.config.queries.clone();
        let reaction_id = self.config.id.clone();

        tokio::spawn(async move {
            while let Some(query_result) = result_rx.recv().await {
                // Filter results based on configured queries
                if !query_filter.contains(&query_result.query_id) {
                    continue;
                }

                // Mock reaction processing - just log the result
                info!(
                    "Reaction '{}' processing result from query {}: {} results",
                    reaction_id,
                    query_result.query_id,
                    query_result.results.len()
                );

                // Simulate some processing time
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });

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
    application_handles: Arc<RwLock<HashMap<String, ApplicationReactionHandle>>>,
}

impl ReactionManager {
    pub fn new(event_tx: ComponentEventSender) -> Self {
        Self {
            reactions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            application_handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_application_handle(&self, name: &str) -> Option<ApplicationReactionHandle> {
        self.application_handles.read().await.get(name).cloned()
    }

    pub async fn add_reaction(&self, config: ReactionConfig) -> Result<()> {
        self.add_reaction_internal(config).await
    }

    pub async fn add_reaction_without_save(&self, config: ReactionConfig) -> Result<()> {
        self.add_reaction_internal(config).await
    }

    async fn add_reaction_internal(&self, config: ReactionConfig ) -> Result<()> {
        // Check if reaction with this id already exists
        if self.reactions.read().await.contains_key(&config.id) {
            return Err(anyhow::anyhow!(
                "Reaction with id '{}' already exists",
                config.id
            ));
        }

        let reaction: Arc<dyn Reaction> = match config.reaction_type.as_str() {
            "log" => Arc::new(LogReaction::new(config.clone(), self.event_tx.clone())),
            "http" => Arc::new(HttpReaction::new(config.clone(), self.event_tx.clone())),
            // Adaptive HTTP reaction
            "http_adaptive" | "adaptive_http" => Arc::new(AdaptiveHttpReaction::new(
                config.clone(),
                self.event_tx.clone(),
            )),
            "grpc" => Arc::new(GrpcReaction::new(config.clone(), self.event_tx.clone())),
            "sse" => Arc::new(SseReaction::new(config.clone(), self.event_tx.clone())),
            // Adaptive gRPC reaction
            "grpc_adaptive" | "adaptive_grpc" => Arc::new(AdaptiveGrpcReaction::new(
                config.clone(),
                self.event_tx.clone(),
            )),
            "platform" => Arc::new(PlatformReaction::new(config.clone(), self.event_tx.clone())?),
            "profiler" => Arc::new(ProfilerReaction::new(config.clone(), self.event_tx.clone())),
            "application" => {
                let (app_reaction, handle) =
                    ApplicationReaction::new(config.clone(), self.event_tx.clone());
                // Store the handle for the application to use
                self.application_handles
                    .write()
                    .await
                    .insert(config.id.clone(), handle);
                Arc::new(app_reaction)
            }
            _ => Arc::new(MockReaction::new(config.clone(), self.event_tx.clone())),
        };

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

    pub async fn start_reaction(&self, id: String, result_rx: QueryResultReceiver) -> Result<()> {
        let reactions = self.reactions.read().await;
        if let Some(reaction) = reactions.get(&id) {
            let status = reaction.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            reaction.start(result_rx).await?;
        } else {
            return Err(anyhow::anyhow!("Reaction not found: {}", id));
        }

        Ok(())
    }

    pub async fn stop_reaction(&self, id: String) -> Result<()> {
        let reactions = self.reactions.read().await;
        if let Some(reaction) = reactions.get(&id) {
            let status = reaction.status().await;
            is_operation_valid(&status, &Operation::Stop).map_err(|e| anyhow::anyhow!(e))?;
            reaction.stop().await?;
        } else {
            return Err(anyhow::anyhow!("Reaction not found: {}", id));
        }

        Ok(())
    }

    pub async fn get_reaction_status(&self, id: String) -> Result<ComponentStatus> {
        let reactions = self.reactions.read().await;
        if let Some(reaction) = reactions.get(&id) {
            Ok(reaction.status().await)
        } else {
            Err(anyhow::anyhow!("Reaction not found: {}", id))
        }
    }

    pub async fn get_reaction(&self, id: String) -> Result<ReactionRuntime> {
        let reactions = self.reactions.read().await;
        if let Some(reaction) = reactions.get(&id) {
            let status = reaction.status().await;
            let config = reaction.get_config();
            let runtime = ReactionRuntime {
                id: config.id.clone(),
                reaction_type: config.reaction_type.clone(),
                status: status.clone(),
                error_message: match &status {
                    ComponentStatus::Error => Some("Reaction error occurred".to_string()),
                    _ => None,
                },
                queries: config.queries.clone(),
                properties: config.properties.clone(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Reaction not found: {}", id))
        }
    }

    pub async fn update_reaction(&self, id: String, config: ReactionConfig) -> Result<()> {
        let reactions = self.reactions.read().await;
        if let Some(reaction) = reactions.get(&id) {
            let status = reaction.status().await;
            let was_running =
                matches!(status, ComponentStatus::Running | ComponentStatus::Starting);

            // If running, we need to stop it first
            if was_running {
                drop(reactions);
                self.stop_reaction(id.clone()).await?;
                // Re-acquire lock after stop
                let reactions = self.reactions.read().await;
                if let Some(reaction) = reactions.get(&id) {
                    let status = reaction.status().await;
                    is_operation_valid(&status, &Operation::Update)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                drop(reactions);
            } else {
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
                drop(reactions);
            }

            // For now, update means remove and re-add
            self.delete_reaction(id.clone()).await?;
            self.add_reaction(config).await?;

            // If it was running, restart it
            if was_running {
                // Need to get a receiver for the reaction
                let (_tx, rx) = tokio::sync::mpsc::channel(100);
                self.start_reaction(id, rx).await?;
            }
        } else {
            return Err(anyhow::anyhow!("Reaction not found: {}", id));
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
        let reactions = self.reactions.read().await;
        let mut result = Vec::new();

        for (id, reaction) in reactions.iter() {
            let status = reaction.status().await;
            result.push((id.clone(), status));
        }

        result
    }

    pub async fn get_reaction_config(&self, name: &str) -> Option<ReactionConfig> {
        let reactions = self.reactions.read().await;
        reactions.get(name).map(|r| r.get_config().clone())
    }

    pub async fn start_all(&self, subscription_router: &SubscriptionRouter) -> Result<()> {
        let reactions = self.reactions.read().await;
        let mut failed_reactions = Vec::new();

        for (id, reaction) in reactions.iter() {
            let config = reaction.get_config();
            if config.auto_start {
                // Get a receiver connected to the subscription router
                match subscription_router
                    .add_reaction_subscription(id.clone(), config.queries.clone())
                    .await
                {
                    Ok(rx) => {
                        info!("Auto-starting reaction: {}", id);
                        if let Err(e) = reaction.start(rx).await {
                            error!("Failed to start reaction {}: {}", id, e);
                            failed_reactions.push((id.clone(), e.to_string()));
                            // Continue starting other reactions instead of returning early
                        }
                    }
                    Err(e) => {
                        error!("Failed to create subscription for reaction {}: {}", id, e);
                        failed_reactions.push((id.clone(), e));
                    }
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
