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

use crate::channels::ComponentStatus;
use crate::component_graph::{ComponentGraph, ComponentKind};
use crate::config::RuntimeConfig;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;

/// Manages the lifecycle orchestration for DrasiLib components
///
/// This module handles:
/// - Loading configuration and creating components
/// - Starting components in dependency order (Sources → Queries → Reactions)
/// - Stopping components in reverse order (Reactions → Queries → Sources)
///
/// Event recording (component status history) is handled by the graph update loop
/// in [`DrasiLib`], which records events directly into each manager's
/// [`ComponentEventHistory`] after applying status updates to the graph.
pub(crate) struct LifecycleManager {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    graph: Arc<RwLock<ComponentGraph>>,
}

impl LifecycleManager {
    /// Create a new lifecycle manager
    pub fn new(
        config: Arc<RuntimeConfig>,
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
        graph: Arc<RwLock<ComponentGraph>>,
    ) -> Self {
        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            graph,
        }
    }

    /// Load configuration from RuntimeConfig and create all components
    ///
    /// This method creates queries from configuration without starting them.
    /// Note: Sources and reactions must be added as instances via the builder or API.
    pub async fn load_configuration(&self) -> Result<()> {
        info!("Loading configuration");

        // Load queries (only queries use config-based creation)
        for query_config in &self.config.queries {
            let config = query_config.clone();

            // Step 1: Register in the component graph (sources must already exist)
            {
                let mut graph = self.graph.write().await;
                let mut metadata = HashMap::new();
                metadata.insert("query".to_string(), config.query.clone());
                let source_ids: Vec<String> =
                    config.sources.iter().map(|s| s.source_id.clone()).collect();
                graph.register_query(&config.id, metadata, &source_ids)?;
            }

            // Step 2: Provision runtime (with rollback on failure)
            let query_id = config.id.clone();
            if let Err(e) = self.query_manager.provision_query(config).await {
                let mut graph = self.graph.write().await;
                let _ = graph.deregister(&query_id);
                return Err(e);
            }
        }

        info!("Configuration loaded successfully");
        Ok(())
    }

    /// Start all components with auto_start enabled
    ///
    /// Components are started in dependency order: Sources → Queries → Reactions
    /// Only components with auto_start=true will be started.
    pub async fn start_components(&self) -> Result<()> {
        info!("Starting all auto-start components in sequence: Sources → Queries → Reactions");

        // Start sources first (SourceManager.start_all() checks auto_start internally)
        info!("Starting auto-start sources");
        self.source_manager.start_all().await?;
        info!("All auto-start sources started successfully");

        // Start queries after sources
        // QueryManager.start_all() reads from graph and checks auto_start internally
        info!("Starting auto-start queries");
        self.query_manager.start_all().await?;
        info!("All auto-start queries started successfully");

        // Start reactions after queries
        // ReactionManager.start_all() handles auto_start logic internally
        info!("Starting auto-start reactions");
        self.reaction_manager.start_all().await?;
        info!("All auto-start reactions started successfully");

        info!("All auto-start components started in sequence: Sources → Queries → Reactions");
        info!("[STARTUP-COMPLETE] DrasiLib.start() is now returning - all components and subscriptions should be active");
        Ok(())
    }

    /// Stop all running components
    ///
    /// Components are stopped in reverse dependency order using the graph's
    /// topological ordering: Reactions → Queries → Sources.
    /// Errors during shutdown are logged but don't prevent other components from stopping.
    pub async fn stop_all_components(&self) -> Result<()> {
        use log::error;

        // Get a single consistent snapshot of components in reverse dependency order
        let shutdown_order: Vec<(String, ComponentKind, ComponentStatus)> = {
            let graph = self.graph.read().await;
            match graph.topological_order() {
                Ok(order) => order
                    .into_iter()
                    .rev()
                    .map(|n| (n.id.clone(), n.kind.clone(), n.status))
                    .collect(),
                Err(e) => {
                    error!("Failed to compute topological order: {e}, falling back to kind-based ordering");
                    // Fallback: list by kind in reverse order
                    let mut all = Vec::new();
                    for (id, status) in graph.list_by_kind(&ComponentKind::Reaction) {
                        all.push((id, ComponentKind::Reaction, status));
                    }
                    for (id, status) in graph.list_by_kind(&ComponentKind::Query) {
                        all.push((id, ComponentKind::Query, status));
                    }
                    for (id, status) in graph.list_by_kind(&ComponentKind::Source) {
                        all.push((id, ComponentKind::Source, status));
                    }
                    all
                }
            }
        };

        for (id, kind, status) in shutdown_order {
            if !matches!(status, ComponentStatus::Running | ComponentStatus::Starting) {
                continue;
            }

            let result = match kind {
                ComponentKind::Reaction => {
                    info!("Stopping reaction '{id}'");
                    self.reaction_manager.stop_reaction(id.clone()).await
                }
                ComponentKind::Query => {
                    info!("Stopping query '{id}'");
                    self.query_manager.stop_query(id.clone()).await
                }
                ComponentKind::Source => {
                    info!("Stopping source '{id}'");
                    self.source_manager.stop_source(id.clone()).await
                }
                _ => continue,
            };

            if let Err(e) = result {
                error!("Error stopping {kind} {id}: {e}");
            }
        }

        info!("All components stopped");
        Ok(())
    }
}
