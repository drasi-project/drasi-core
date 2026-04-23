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
    ///
    /// All components are attempted even if some fail. Returns an aggregated
    /// error listing all failures, or `Ok(())` if all stopped successfully.
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

        let mut failures = Vec::new();

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
                failures.push((id, e.to_string()));
            }
        }

        if failures.is_empty() {
            info!("All components stopped");
            Ok(())
        } else {
            let error_msg = failures
                .iter()
                .map(|(id, err)| format!("{id}: {err}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "Failed to stop some components: {error_msg}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::channels::ComponentStatus;
    use crate::lib_core::DrasiLib;
    use crate::sources::tests::TestMockSource;
    use crate::sources::COMPONENT_GRAPH_SOURCE_ID;
    use crate::test_helpers::wait_for_component_status;
    use std::time::Duration;

    /// Helper: build a DrasiLib with the given sources (not yet started).
    async fn build_with_sources(sources: Vec<TestMockSource>) -> DrasiLib {
        let mut builder = DrasiLib::builder().with_id("lifecycle-test");
        for s in sources {
            builder = builder.with_source(s);
        }
        builder.build().await.unwrap()
    }

    // ========================================================================
    // start_components
    // ========================================================================

    #[tokio::test]
    async fn start_components_starts_auto_start_sources() {
        let source = TestMockSource::with_auto_start("auto-src".to_string(), true).unwrap();
        let core = build_with_sources(vec![source]).await;

        let mut event_rx = core.component_graph.read().await.subscribe();
        core.start().await.unwrap();

        wait_for_component_status(
            &mut event_rx,
            "auto-src",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        let status = core.get_source_status("auto-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn start_components_skips_non_auto_start() {
        let source = TestMockSource::with_auto_start("manual-src".to_string(), false).unwrap();
        let core = build_with_sources(vec![source]).await;

        core.start().await.unwrap();

        // Non-autostart source should remain Added
        let status = core.get_source_status("manual-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);
    }

    // ========================================================================
    // stop_all_components
    // ========================================================================

    #[tokio::test]
    async fn stop_all_components_stops_running() {
        let source = TestMockSource::with_auto_start("stop-src".to_string(), true).unwrap();
        let core = build_with_sources(vec![source]).await;

        let mut event_rx = core.component_graph.read().await.subscribe();
        core.start().await.unwrap();

        wait_for_component_status(
            &mut event_rx,
            "stop-src",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        // Now stop
        core.stop().await.unwrap();

        wait_for_component_status(
            &mut event_rx,
            "stop-src",
            ComponentStatus::Stopped,
            Duration::from_secs(5),
        )
        .await;

        let status = core.get_source_status("stop-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn stop_all_components_handles_already_stopped() {
        let source = TestMockSource::with_auto_start("idle-src".to_string(), false).unwrap();
        let core = build_with_sources(vec![source]).await;

        core.start().await.unwrap();

        // Source is not auto-started, so it's still in Added state.
        // Stopping should succeed without errors for user components.
        // Internal components may fail, so we just verify the server marks as stopped.
        let _ = core.stop().await;

        let status = core.get_source_status("idle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);
    }

    // ========================================================================
    // load_configuration
    // ========================================================================

    #[tokio::test]
    async fn load_configuration_creates_queries_from_config() {
        use crate::builder::Query;

        // Build with a source and a query that references it
        let source = TestMockSource::with_auto_start("cfg-src".to_string(), true).unwrap();
        let core = DrasiLib::builder()
            .with_id("cfg-test")
            .with_source(source)
            .with_query(
                Query::cypher("cfg-query")
                    .query("MATCH (n:Person) RETURN n")
                    .from_source("cfg-src")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // After build(), load_configuration has already run.
        // The query should exist in the list.
        let queries = core.list_queries().await.unwrap();
        assert!(
            queries.iter().any(|(id, _)| id == "cfg-query"),
            "Query 'cfg-query' should exist after build; got: {queries:?}"
        );

        // Query should be in Added state (not yet started)
        let (_, status) = queries.iter().find(|(id, _)| id == "cfg-query").unwrap();
        assert_eq!(*status, ComponentStatus::Added);
    }
}
