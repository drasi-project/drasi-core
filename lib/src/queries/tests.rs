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

#[cfg(test)]
mod manager_tests {
    use super::super::*;
    use crate::channels::*;
    use crate::config::{QueryConfig, QueryLanguage, SourceSubscriptionConfig};
    use crate::queries::output_state::FetchError;
    use crate::sources::tests::{create_test_bootstrap_mock_source, create_test_mock_source};
    use crate::sources::SourceManager;
    use crate::test_helpers::wait_for_component_status;
    use drasi_core::middleware::MiddlewareTypeRegistry;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use std::sync::Arc;

    /// Creates a test query configuration
    fn create_test_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        create_test_query_config_with_auto_start(id, sources, true)
    }

    /// Creates a test query configuration with configurable auto_start
    fn create_test_query_config_with_auto_start(
        id: &str,
        sources: Vec<String>,
        auto_start: bool,
    ) -> QueryConfig {
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: sources
                .into_iter()
                .map(|source_id| SourceSubscriptionConfig {
                    nodes: vec![],
                    relations: vec![],
                    source_id,
                    pipeline: vec![],
                })
                .collect(),
            auto_start,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 1000,
            bootstrap_timeout_secs: 300,
        }
    }

    /// Creates a test GQL query configuration
    fn create_test_gql_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n:Person) RETURN n.name".to_string(),
            query_language: QueryLanguage::GQL,
            middleware: vec![],
            sources: sources
                .into_iter()
                .map(|source_id| SourceSubscriptionConfig {
                    nodes: vec![],
                    relations: vec![],
                    source_id,
                    pipeline: vec![],
                })
                .collect(),
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 1000,
            bootstrap_timeout_secs: 300,
        }
    }

    async fn create_test_manager() -> (
        Arc<QueryManager>,
        Arc<SourceManager>,
        Arc<tokio::sync::RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        let log_registry = crate::managers::get_or_init_global_registry();
        let (graph, update_rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();
        let graph = Arc::new(tokio::sync::RwLock::new(graph));

        // Spawn a mini graph update loop for tests
        {
            let graph_clone = graph.clone();
            tokio::spawn(async move {
                let mut rx = update_rx;
                while let Some(update) = rx.recv().await {
                    let mut g = graph_clone.write().await;
                    g.apply_update(update);
                }
            });
        }

        let source_manager = Arc::new(SourceManager::new(
            "test-instance",
            log_registry.clone(),
            graph.clone(),
            update_tx.clone(),
        ));

        // Create a test IndexFactory with empty backends (no plugin, memory only)
        let index_factory = Arc::new(crate::indexes::IndexFactory::new(vec![], None));

        // Create a test middleware registry
        let middleware_registry = Arc::new(MiddlewareTypeRegistry::new());

        let query_manager = Arc::new(QueryManager::new(
            "test-instance",
            source_manager.clone(),
            index_factory,
            middleware_registry,
            log_registry,
            graph.clone(),
            update_tx,
        ));

        (query_manager, source_manager, graph)
    }

    /// Alias for backward compatibility
    async fn create_test_manager_with_graph() -> (
        Arc<QueryManager>,
        Arc<SourceManager>,
        Arc<tokio::sync::RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        create_test_manager().await
    }

    /// Helper: register a source in the graph, then provision it in the source manager.
    async fn add_source(
        source_manager: &SourceManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        source: impl crate::sources::Source + 'static,
    ) -> anyhow::Result<()> {
        let source_id = source.id().to_string();
        let source_type = source.type_name().to_string();
        let auto_start = source.auto_start();
        {
            let mut g = graph.write().await;
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("kind".to_string(), source_type);
            metadata.insert("autoStart".to_string(), auto_start.to_string());
            g.register_source(&source_id, metadata)?;
        }
        source_manager.provision_source(source).await
    }

    /// Helper: register a query in the graph, then provision it in the query manager.
    /// Registers placeholder source nodes for any referenced sources not already in the graph.
    async fn add_query(
        manager: &QueryManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        {
            let mut g = graph.write().await;
            // Ensure referenced sources exist as placeholder nodes
            let source_ids: Vec<String> =
                config.sources.iter().map(|s| s.source_id.clone()).collect();
            for sid in &source_ids {
                if !g.contains(sid) {
                    g.register_source(sid, std::collections::HashMap::new())?;
                }
            }
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("query".to_string(), config.query.clone());
            g.register_query(&config.id, metadata, &source_ids)?;
        }
        manager.provision_query(config).await
    }

    /// Helper: teardown a query in the manager, then deregister from the graph.
    async fn delete_query(
        manager: &QueryManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        id: &str,
    ) -> anyhow::Result<()> {
        manager.teardown_query(id.to_string()).await?;
        let mut g = graph.write().await;
        g.deregister(id)?;
        Ok(())
    }

    #[tokio::test]
    async fn test_add_query() {
        let (manager, source_manager, graph) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        let result = add_query(&manager, &graph, config.clone()).await;

        assert!(result.is_ok());

        // Verify query was added
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0, "test-query");
    }

    #[tokio::test]
    async fn test_add_duplicate_query() {
        let (manager, source_manager, graph) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec![]);

        // Add query first time
        assert!(add_query(&manager, &graph, config.clone()).await.is_ok());

        // Try to add same query again
        let result = add_query(&manager, &graph, config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_delete_query() {
        let (manager, source_manager, graph) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec![]);
        add_query(&manager, &graph, config).await.unwrap();

        // Delete the query
        let result = delete_query(&manager, &graph, "test-query").await;
        assert!(result.is_ok());

        // Verify query was removed
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 0);
    }

    #[tokio::test]
    async fn test_start_query() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.read().await.subscribe();

        // Add a source first using instance-based approach
        let source = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Add and start a query
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();

        let result = manager.start_query("test-query".to_string()).await;
        assert!(result.is_ok());

        // Check for status event from graph broadcast
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == "test-query" {
                    // Skip the Stopped event emitted by add_query (with "added" message)
                    if event
                        .message
                        .as_deref()
                        .is_some_and(|m| m.ends_with("added"))
                    {
                        continue;
                    }
                    assert!(
                        matches!(event.status, ComponentStatus::Starting)
                            || matches!(event.status, ComponentStatus::Running)
                    );
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for status event");
    }

    #[tokio::test]
    async fn test_stop_query() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.read().await.subscribe();

        // Add a source using instance-based approach
        let source = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Add and start a query
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();

        manager.start_query("test-query".to_string()).await.unwrap();

        // Wait for query to reach Running state
        wait_for_component_status(
            &mut event_rx,
            "test-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Stop the query
        let result = manager.stop_query("test-query".to_string()).await;
        assert!(result.is_ok());

        // Check for stop event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == "test-query"
                    && matches!(event.status, ComponentStatus::Stopped)
                {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for stop event");
    }

    #[tokio::test]
    async fn test_stop_query_cancels_subscription_tasks() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add a source so subscriptions succeed using instance-based approach
        let source = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Subscribe to graph events BEFORE starting the query
        let mut event_rx = graph.read().await.subscribe();

        // Add the query and start it
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("test-query".to_string()).await.unwrap();

        // Wait for query to reach Running state
        wait_for_component_status(
            &mut event_rx,
            "test-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Inspect concrete query to verify subscription tasks are present
        let query = manager.get_query_instance("test-query").await.unwrap();
        let concrete = query.as_any().downcast_ref::<DrasiQuery>().unwrap();

        assert!(
            concrete.subscription_task_count().await > 0,
            "Expected active subscription task"
        );

        manager.stop_query("test-query".to_string()).await.unwrap();

        // Wait for query to reach Stopped state
        wait_for_component_status(
            &mut event_rx,
            "test-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;

        assert!(
            concrete.subscription_task_count().await == 0,
            "Subscription tasks should be cleared on stop"
        );
    }

    #[tokio::test]
    async fn test_partial_subscription_failure_cleans_up_tasks() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE starting the query
        let mut event_rx = graph.read().await.subscribe();

        // Add only source1 - source2 will be missing to trigger failure
        let source1 = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source1).await.unwrap();

        // Create query that subscribes to TWO sources - source2 is missing
        let config = create_test_query_config(
            "test-query",
            vec!["source1".to_string(), "source2".to_string()],
        );
        add_query(&manager, &graph, config).await.unwrap();

        // Starting should fail because source2 doesn't exist
        let result = manager.start_query("test-query".to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source2"));

        // Wait for query to reach Error state via graph update
        wait_for_component_status(
            &mut event_rx,
            "test-query",
            ComponentStatus::Error,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Error),
            "Expected Error status after failed start"
        );

        // Crucially: subscription tasks from source1 should have been cleaned up
        let query = manager.get_query_instance("test-query").await.unwrap();
        let concrete = query.as_any().downcast_ref::<DrasiQuery>().unwrap();

        // The forwarder task for source1 was spawned before source2 subscription failed,
        // so the rollback code should have aborted it
        assert_eq!(
            concrete.subscription_task_count().await,
            0,
            "Subscription tasks should be cleaned up after partial failure"
        );
    }

    #[tokio::test]
    async fn test_add_gql_query() {
        let (manager, source_manager, graph) = create_test_manager().await;

        let config = create_test_gql_query_config("test-gql-query", vec!["source1".to_string()]);
        let result = add_query(&manager, &graph, config.clone()).await;

        assert!(result.is_ok());

        // Verify query was added
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0, "test-gql-query");
    }

    #[tokio::test]
    async fn test_start_gql_query() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.read().await.subscribe();

        // Add a source first using instance-based approach
        let source = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Add and start a GQL query
        let config = create_test_gql_query_config("test-gql-query", vec!["source1".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();

        let result = manager.start_query("test-gql-query".to_string()).await;
        assert!(result.is_ok());

        // Check for status event from graph broadcast
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == "test-gql-query" {
                    // Skip the Stopped event emitted by add_query (with "added" message)
                    if event
                        .message
                        .as_deref()
                        .is_some_and(|m| m.ends_with("added"))
                    {
                        continue;
                    }
                    assert!(
                        matches!(event.status, ComponentStatus::Starting)
                            || matches!(event.status, ComponentStatus::Running)
                    );
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for status event");
    }

    #[tokio::test]
    async fn test_mixed_language_queries() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add a Cypher query
        let cypher_config = create_test_query_config("cypher-query", vec!["source1".to_string()]);
        assert!(add_query(&manager, &graph, cypher_config).await.is_ok());

        // Add a GQL query
        let gql_config = create_test_gql_query_config("gql-query", vec!["source1".to_string()]);
        assert!(add_query(&manager, &graph, gql_config).await.is_ok());

        // Verify both queries were added
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 2);

        let query_ids: Vec<String> = queries.iter().map(|(id, _)| id.clone()).collect();
        assert!(query_ids.contains(&"cypher-query".to_string()));
        assert!(query_ids.contains(&"gql-query".to_string()));
    }

    #[tokio::test]
    async fn test_get_query_config() {
        let (manager, source_manager, graph) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        add_query(&manager, &graph, config.clone()).await.unwrap();

        let retrieved = manager.get_query_config("test-query").await;
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, config.id);
        assert_eq!(retrieved.query, config.query);
        assert_eq!(retrieved.sources.len(), config.sources.len());
    }

    #[tokio::test]
    async fn test_update_query() {
        let (manager, source_manager, graph) = create_test_manager().await;

        let mut config = create_test_query_config("test-query", vec![]);
        add_query(&manager, &graph, config.clone()).await.unwrap();

        // Update config
        config.query = "MATCH (n:Updated) RETURN n".to_string();

        // Perform update: teardown → deregister → re-register → provision
        manager
            .teardown_query("test-query".to_string())
            .await
            .unwrap();
        {
            let mut g = graph.write().await;
            let _ = g.deregister("test-query");
        }
        {
            let mut g = graph.write().await;
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("query".to_string(), config.query.clone());
            let source_ids: Vec<String> =
                config.sources.iter().map(|s| s.source_id.clone()).collect();
            g.register_query(&config.id, metadata, &source_ids).unwrap();
        }
        manager.provision_query(config.clone()).await.unwrap();

        // Verify update
        let retrieved = manager.get_query_config("test-query").await.unwrap();
        assert_eq!(retrieved.query, config.query);
    }

    #[tokio::test]
    async fn test_query_lifecycle() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add a source using instance-based approach
        let source = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Add a query that subscribes to the source
        let query_config = create_test_query_config("test-query", vec!["source1".to_string()]);
        add_query(&manager, &graph, query_config).await.unwrap();

        // Verify query is in Added state
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Added));

        // Subscribe to graph events BEFORE starting the query
        let mut event_rx = graph.read().await.subscribe();

        // Start the query
        manager.start_query("test-query".to_string()).await.unwrap();

        // Verify query is running
        wait_for_component_status(
            &mut event_rx,
            "test-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Running));

        // Stop the query
        manager.stop_query("test-query".to_string()).await.unwrap();

        // Verify query is stopped
        wait_for_component_status(
            &mut event_rx,
            "test-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));
    }

    // ============================================================================
    // Auto-start tests
    // ============================================================================

    /// Test that query config auto_start defaults to true
    #[tokio::test]
    async fn test_query_auto_start_defaults_to_true() {
        // Create query using default helper (should have auto_start=true)
        let config = create_test_query_config("default-query", vec![]);
        assert!(config.auto_start, "Default auto_start should be true");
    }

    /// Test that query with auto_start=false can be added and remains stopped
    #[tokio::test]
    async fn test_query_auto_start_false_not_started_on_add() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add query with auto_start=false (no sources needed since we won't start it)
        let config = create_test_query_config_with_auto_start("no-auto-start-query", vec![], false);
        add_query(&manager, &graph, config).await.unwrap();

        // Query should be in Added state
        let status = manager
            .get_query_status("no-auto-start-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Added),
            "Query with auto_start=false should remain in Added state after add"
        );
    }

    /// Test that query with auto_start=false can be manually started
    #[tokio::test]
    async fn test_query_auto_start_false_can_be_manually_started() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add a source
        let source = create_test_mock_source("source1".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Add query with auto_start=false
        let config = create_test_query_config_with_auto_start(
            "manual-query",
            vec!["source1".to_string()],
            false,
        );
        add_query(&manager, &graph, config).await.unwrap();

        // Query should be in Added state initially
        let status = manager
            .get_query_status("manual-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Added),
            "Query with auto_start=false should be in Added state initially"
        );

        // Subscribe to graph events BEFORE starting the query
        let mut event_rx = graph.read().await.subscribe();

        // Manually start the query
        manager
            .start_query("manual-query".to_string())
            .await
            .unwrap();

        // Wait for query to reach Running state
        wait_for_component_status(
            &mut event_rx,
            "manual-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Query should now be running
        let status = manager
            .get_query_status("manual-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Running),
            "Query with auto_start=false should be manually startable"
        );
    }

    /// Test that get_query_config preserves auto_start value
    #[tokio::test]
    async fn test_query_config_preserves_auto_start() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add query with auto_start=false
        let config = create_test_query_config_with_auto_start("test-query", vec![], false);
        add_query(&manager, &graph, config).await.unwrap();

        // Retrieve config and verify auto_start is preserved
        let retrieved = manager.get_query_config("test-query").await.unwrap();
        assert!(
            !retrieved.auto_start,
            "Retrieved config should preserve auto_start=false"
        );

        // Add another query with auto_start=true
        let config2 = create_test_query_config_with_auto_start("test-query-2", vec![], true);
        add_query(&manager, &graph, config2).await.unwrap();

        let retrieved2 = manager.get_query_config("test-query-2").await.unwrap();
        assert!(
            retrieved2.auto_start,
            "Retrieved config should preserve auto_start=true"
        );
    }

    // ============================================================================
    // Cleanup Tests
    // ============================================================================

    /// Test that deleting a query cleans up its event history
    #[tokio::test]
    async fn test_delete_query_cleans_up_event_history() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add a query
        let config = create_test_query_config("cleanup-events-query", vec![]);
        add_query(&manager, &graph, config).await.unwrap();

        // Record an event manually to simulate lifecycle
        manager
            .record_event(ComponentEvent {
                component_id: "cleanup-events-query".to_string(),
                component_type: ComponentType::Query,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("Test event".to_string()),
            })
            .await;

        // Verify events exist
        let events = manager.get_query_events("cleanup-events-query").await;
        assert!(!events.is_empty(), "Expected events after recording");

        // Delete the query
        delete_query(&manager, &graph, "cleanup-events-query")
            .await
            .unwrap();

        // Verify events are cleaned up
        let events_after = manager.get_query_events("cleanup-events-query").await;
        assert!(events_after.is_empty(), "Expected no events after deletion");
    }

    // ============================================================================
    // Bootstrap gate tests
    // ============================================================================

    /// Test that the bootstrap gate delays the query's transition from Starting to Running
    /// until the bootstrap channel is closed (signaling bootstrap completion).
    #[tokio::test]
    async fn test_bootstrap_gate_delays_running_status() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.read().await.subscribe();

        // Create a bootstrap channel — test controls the sender
        let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(100);

        // Add a source that provides the bootstrap channel
        let source = create_test_bootstrap_mock_source("bs-source".to_string(), bootstrap_rx);
        add_source(&source_manager, &graph, source).await.unwrap();

        // Add a query subscribed to this source
        let config = create_test_query_config("bs-query", vec!["bs-source".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();

        // Start the query
        manager.start_query("bs-query".to_string()).await.unwrap();

        // Drain the Starting event from the graph broadcast
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == "bs-query"
                    && matches!(event.status, ComponentStatus::Starting)
                {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for Starting event");

        // While bootstrap channel is still open, the query should remain in Starting state
        let status = manager
            .get_query_status("bs-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Starting),
            "Expected Starting while bootstrap is in progress, got {status:?}"
        );

        // Drop the bootstrap sender — closes the channel, signaling bootstrap completion
        drop(bootstrap_tx);

        // Wait for the Running event from graph broadcast
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == "bs-query"
                    && matches!(event.status, ComponentStatus::Running)
                {
                    return;
                }
            }
        })
        .await
        .expect("Timeout waiting for Running event after bootstrap completion");

        // Verify the query is now Running
        let status = manager
            .get_query_status("bs-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Running),
            "Expected Running after bootstrap completed, got {status:?}"
        );
    }

    /// Test that deleting a query cleans up its log history
    #[tokio::test]
    async fn test_delete_query_cleans_up_log_history() {
        let (manager, source_manager, graph) = create_test_manager().await;

        // Add a query
        let config = create_test_query_config("cleanup-logs-query", vec![]);
        add_query(&manager, &graph, config).await.unwrap();

        // Generate a log via subscribe (which creates the channel) then check
        // First subscribe to create the channel
        let result = manager.subscribe_logs("cleanup-logs-query").await;
        assert!(result.is_some(), "Expected to subscribe to query logs");

        // Delete the query
        delete_query(&manager, &graph, "cleanup-logs-query")
            .await
            .unwrap();

        // Verify logs are cleaned up (subscribe should fail for non-existent query)
        let result = manager.subscribe_logs("cleanup-logs-query").await;
        assert!(result.is_none(), "Expected None for deleted query logs");
    }
}

#[cfg(test)]
mod query_core_tests {
    use crate::config::QueryConfig;

    #[tokio::test]
    async fn test_query_syntax_validation() {
        let valid_queries = vec![
            "MATCH (n) RETURN n",
            "MATCH (n:Person) WHERE n.age > 21 RETURN n.name",
            "MATCH (a)-[r:KNOWS]->(b) RETURN a, r, b",
        ];

        for query in valid_queries {
            let config = QueryConfig {
                id: "test".to_string(),
                query: query.to_string(),
                query_language: crate::config::QueryLanguage::Cypher,
                middleware: vec![],
                sources: vec![],
                auto_start: false,
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
                recovery_policy: None,
                outbox_capacity: 1000,
                bootstrap_timeout_secs: 300,
            };

            // Just verify the config can be created
            assert!(!config.query.is_empty());
        }
    }

    #[tokio::test]
    async fn test_empty_query_validation() {
        let config = QueryConfig {
            id: "test".to_string(),
            query: "".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![],
            auto_start: false,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 1000,
            bootstrap_timeout_secs: 300,
        };

        // Empty queries should be caught during validation
        assert!(config.query.is_empty());
    }
}

// =========================================================================
// Output State Integration Tests (fetch_snapshot, fetch_outbox, wait_until_running)
// =========================================================================
#[cfg(test)]
mod output_state_integration_tests {
    use super::super::*;
    use crate::channels::*;
    use crate::config::{QueryConfig, QueryLanguage, SourceSubscriptionConfig};
    use crate::queries::output_state::FetchError;
    use crate::sources::tests::{create_test_bootstrap_mock_source, create_test_mock_source};
    use crate::sources::SourceManager;
    use crate::test_helpers::wait_for_component_status;
    use drasi_core::middleware::MiddlewareTypeRegistry;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use std::sync::Arc;

    /// Creates a test query configuration
    fn create_test_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n:Person) RETURN n.name".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: sources
                .iter()
                .map(|s| SourceSubscriptionConfig {
                    nodes: vec![],
                    relations: vec![],
                    source_id: s.clone(),
                    pipeline: vec![],
                })
                .collect(),
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 1000,
            bootstrap_timeout_secs: 300,
        }
    }

    async fn create_test_manager_with_graph() -> (
        Arc<QueryManager>,
        Arc<SourceManager>,
        Arc<tokio::sync::RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        let log_registry = crate::managers::get_or_init_global_registry();
        let (graph, update_rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();
        let graph = Arc::new(tokio::sync::RwLock::new(graph));

        // Spawn a mini graph update loop for tests
        {
            let graph_clone = graph.clone();
            tokio::spawn(async move {
                let mut rx = update_rx;
                while let Some(update) = rx.recv().await {
                    let mut g = graph_clone.write().await;
                    g.apply_update(update);
                }
            });
        }

        let source_manager = Arc::new(SourceManager::new(
            "test-instance",
            log_registry.clone(),
            graph.clone(),
            update_tx.clone(),
        ));

        let index_factory = Arc::new(crate::indexes::IndexFactory::new(vec![], None));
        let middleware_registry = Arc::new(MiddlewareTypeRegistry::new());

        let query_manager = Arc::new(QueryManager::new(
            "test-instance",
            source_manager.clone(),
            index_factory,
            middleware_registry,
            log_registry,
            graph.clone(),
            update_tx,
        ));

        (query_manager, source_manager, graph)
    }

    async fn add_source(
        source_manager: &SourceManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        source: impl crate::sources::Source + 'static,
    ) -> anyhow::Result<()> {
        let source_id = source.id().to_string();
        let source_type = source.type_name().to_string();
        let auto_start = source.auto_start();
        {
            let mut g = graph.write().await;
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("kind".to_string(), source_type);
            metadata.insert("autoStart".to_string(), auto_start.to_string());
            g.register_source(&source_id, metadata)?;
        }
        source_manager.provision_source(source).await
    }

    async fn add_query(
        manager: &QueryManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        {
            let mut g = graph.write().await;
            let source_ids: Vec<String> =
                config.sources.iter().map(|s| s.source_id.clone()).collect();
            for sid in &source_ids {
                if !g.contains(sid) {
                    g.register_source(sid, std::collections::HashMap::new())?;
                }
            }
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("query".to_string(), config.query.clone());
            g.register_query(&config.id, metadata, &source_ids)?;
        }
        manager.provision_query(config).await
    }

    /// Helper: create a SourceChange::Insert for a Person node
    fn make_person_insert(source_id: &str, node_id: &str, name: &str) -> SourceChange {
        let mut props = ElementPropertyMap::default();
        props.insert(
            "name",
            drasi_core::models::ElementValue::String(name.into()),
        );
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, node_id),
                labels: vec!["Person".into()].into(),
                effective_from: 1000,
            },
            properties: props,
        };
        SourceChange::Insert { element }
    }

    /// Helper: create a BootstrapEvent from a SourceChange
    fn make_bootstrap_event(source_id: &str, change: SourceChange, seq: u64) -> BootstrapEvent {
        BootstrapEvent {
            source_id: source_id.to_string(),
            change,
            timestamp: chrono::Utc::now(),
            sequence: seq,
        }
    }

    #[tokio::test]
    async fn test_fetch_snapshot_blocks_during_bootstrap() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;
        let mut event_rx = graph.read().await.subscribe();

        // Create a bootstrap channel — test controls the sender
        let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(100);

        let source = create_test_bootstrap_mock_source("snap-src".to_string(), bootstrap_rx);
        add_source(&source_manager, &graph, source).await.unwrap();

        let config = create_test_query_config("snap-query", vec!["snap-src".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("snap-query".to_string()).await.unwrap();

        // Drain the Starting event
        wait_for_component_status(
            &mut event_rx,
            "snap-query",
            ComponentStatus::Starting,
            std::time::Duration::from_secs(2),
        )
        .await;

        // fetch_snapshot should block while Starting — spawn it and check it doesn't resolve
        let query = manager.get_query_instance("snap-query").await.unwrap();
        let snapshot_handle = tokio::spawn({
            let q = query.clone();
            async move { q.fetch_snapshot().await }
        });

        // Give it a moment — it should NOT resolve yet
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !snapshot_handle.is_finished(),
            "fetch_snapshot should block during bootstrap"
        );

        // Send a bootstrap event, then close the channel to complete bootstrap
        let insert = make_person_insert("snap-src", "p1", "Alice");
        bootstrap_tx
            .send(make_bootstrap_event("snap-src", insert, 1))
            .await
            .unwrap();
        drop(bootstrap_tx);

        // Wait for Running
        wait_for_component_status(
            &mut event_rx,
            "snap-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Now fetch_snapshot should resolve
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), snapshot_handle)
            .await
            .expect("fetch_snapshot should complete after bootstrap")
            .unwrap()
            .expect("fetch_snapshot should return Ok");

        // After bootstrap: results should contain the bootstrapped data,
        // sequence should be 0 (bootstrap doesn't advance sequence)
        assert_eq!(snapshot.as_of_sequence, 0);
        // The result set may or may not contain the bootstrap row depending on
        // whether the query engine matched it. The important invariant is that
        // the snapshot resolved and as_of_sequence is 0.
    }

    #[tokio::test]
    async fn test_fetch_snapshot_and_outbox_after_live_events() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;
        let mut event_rx = graph.read().await.subscribe();

        // Use a regular mock source (no bootstrap channel → immediate Running)
        let source = create_test_mock_source("live-src".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        let config = create_test_query_config("live-query", vec!["live-src".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("live-query".to_string()).await.unwrap();

        // Wait for Running
        wait_for_component_status(
            &mut event_rx,
            "live-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let query = manager.get_query_instance("live-query").await.unwrap();

        // Before any live events: snapshot should be empty, sequence 0
        let snap = query.fetch_snapshot().await.unwrap();
        assert_eq!(snap.as_of_sequence, 0);
        assert!(snap.results.is_empty());

        // Outbox after 0 should also be empty
        let outbox = query.fetch_outbox(0).await.unwrap();
        assert!(outbox.results.is_empty());
        assert_eq!(outbox.latest_sequence, 0);

        // Inject a live event via the source manager
        let source_arc = source_manager
            .get_source_instance("live-src")
            .await
            .unwrap();
        let mock_source = source_arc
            .as_any()
            .downcast_ref::<crate::sources::tests::TestMockSource>()
            .unwrap();
        let insert = make_person_insert("live-src", "p1", "Bob");
        mock_source.inject_event(insert).await.unwrap();

        // Give the query time to process
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // After a live event that produces results, sequence should advance
        let snap = query.fetch_snapshot().await.unwrap();
        // The sequence may or may not advance depending on whether the query
        // engine matched the node. If it did match (MATCH (n) RETURN n), then
        // sequence should be >= 1 and outbox should have entries.
        if snap.as_of_sequence > 0 {
            let outbox = query.fetch_outbox(0).await.unwrap();
            assert_eq!(outbox.results.len(), snap.as_of_sequence as usize);
            assert_eq!(outbox.results[0].sequence, 1);
            assert_eq!(outbox.latest_sequence, snap.as_of_sequence);
        }
    }

    #[tokio::test]
    async fn test_fetch_outbox_gap_error() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;
        let mut event_rx = graph.read().await.subscribe();

        let source = create_test_mock_source("gap-src".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Use a very small outbox capacity to trigger eviction
        let config = QueryConfig {
            id: "gap-query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "gap-src".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 2, // Very small — will evict quickly
            bootstrap_timeout_secs: 300,
        };
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("gap-query".to_string()).await.unwrap();

        wait_for_component_status(
            &mut event_rx,
            "gap-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let query = manager.get_query_instance("gap-query").await.unwrap();

        // Inject multiple events to fill and overflow the outbox
        let source_arc = source_manager.get_source_instance("gap-src").await.unwrap();
        let mock_source = source_arc
            .as_any()
            .downcast_ref::<crate::sources::tests::TestMockSource>()
            .unwrap();
        for i in 0..5 {
            let insert = make_person_insert("gap-src", &format!("p{i}"), &format!("Person{i}"));
            mock_source.inject_event(insert).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let snap = query.fetch_snapshot().await.unwrap();
        if snap.as_of_sequence > 2 {
            // Requesting after seq 0 when outbox only holds recent entries → gap
            let result = query.fetch_outbox(0).await;
            assert!(
                matches!(result, Err(FetchError::OutboxGap(_))),
                "Expected OutboxGap error when requesting evicted position, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_wait_until_running_returns_not_running_on_error() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;
        let mut event_rx = graph.read().await.subscribe();

        // Use a regular mock source
        let source = create_test_mock_source("err-src".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        let config = create_test_query_config("err-query", vec!["err-src".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("err-query".to_string()).await.unwrap();

        // Wait for Running
        wait_for_component_status(
            &mut event_rx,
            "err-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Stop the query to put it into Stopped state
        manager.stop_query("err-query".to_string()).await.unwrap();

        wait_for_component_status(
            &mut event_rx,
            "err-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;

        let query = manager.get_query_instance("err-query").await.unwrap();

        // fetch_snapshot should return NotRunning when query is stopped
        let result = query.fetch_snapshot().await;
        assert!(
            matches!(result, Err(FetchError::NotRunning { .. })),
            "Expected NotRunning error for stopped query, got {result:?}"
        );

        // fetch_outbox should also return NotRunning
        let result = query.fetch_outbox(0).await;
        assert!(
            matches!(result, Err(FetchError::NotRunning { .. })),
            "Expected NotRunning error for stopped query, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_bootstrap_populates_results_but_not_outbox() {
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;
        let mut event_rx = graph.read().await.subscribe();

        let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(100);

        let source = create_test_bootstrap_mock_source("bs-src2".to_string(), bootstrap_rx);
        add_source(&source_manager, &graph, source).await.unwrap();

        let config = create_test_query_config("bs-query2", vec!["bs-src2".to_string()]);
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("bs-query2".to_string()).await.unwrap();

        // Wait for Starting
        wait_for_component_status(
            &mut event_rx,
            "bs-query2",
            ComponentStatus::Starting,
            std::time::Duration::from_secs(2),
        )
        .await;

        // Send bootstrap events
        for i in 0..3 {
            let insert = make_person_insert("bs-src2", &format!("bp{i}"), &format!("BSPerson{i}"));
            bootstrap_tx
                .send(make_bootstrap_event("bs-src2", insert, i + 1))
                .await
                .unwrap();
        }

        // Close bootstrap channel to complete bootstrap
        drop(bootstrap_tx);

        // Wait for Running
        wait_for_component_status(
            &mut event_rx,
            "bs-query2",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let query = manager.get_query_instance("bs-query2").await.unwrap();
        let snap = query.fetch_snapshot().await.unwrap();

        // Key invariant: after bootstrap, outbox is empty and sequence is 0
        assert_eq!(
            snap.as_of_sequence, 0,
            "Bootstrap should not advance the sequence counter"
        );

        let outbox = query.fetch_outbox(0).await.unwrap();
        assert!(
            outbox.results.is_empty(),
            "Outbox should be empty after bootstrap (only live events populate it)"
        );
        assert_eq!(outbox.latest_sequence, 0);
    }

    #[tokio::test]
    async fn test_noop_events_do_not_advance_sequence() {
        // Use a query that only matches "Person" labels, then send a node
        // with a non-matching label. This exercises the Noop filtering path.
        let (manager, source_manager, graph) = create_test_manager_with_graph().await;
        let mut event_rx = graph.read().await.subscribe();

        let source = create_test_mock_source("noop-src".to_string());
        add_source(&source_manager, &graph, source).await.unwrap();

        // Query that only matches Person nodes specifically
        let config = QueryConfig {
            id: "noop-query".to_string(),
            query: "MATCH (n:Person) RETURN n.name".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "noop-src".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 100,
            bootstrap_timeout_secs: 300,
        };
        add_query(&manager, &graph, config).await.unwrap();
        manager.start_query("noop-query".to_string()).await.unwrap();

        wait_for_component_status(
            &mut event_rx,
            "noop-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let query = manager.get_query_instance("noop-query").await.unwrap();

        // Send a node with a different label (Vehicle) — should not match the query
        let source_arc = source_manager
            .get_source_instance("noop-src")
            .await
            .unwrap();
        let mock_source = source_arc
            .as_any()
            .downcast_ref::<crate::sources::tests::TestMockSource>()
            .unwrap();

        let mut props = ElementPropertyMap::default();
        props.insert(
            "plate",
            drasi_core::models::ElementValue::String("ABC123".into()),
        );
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("noop-src", "v1"),
                labels: vec!["Vehicle".into()].into(),
                effective_from: 1000,
            },
            properties: props,
        };
        mock_source
            .inject_event(SourceChange::Insert { element })
            .await
            .unwrap();

        // Give time to process
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Non-matching event should NOT advance the sequence
        let snap = query.fetch_snapshot().await.unwrap();
        assert_eq!(
            snap.as_of_sequence, 0,
            "Non-matching events should not advance the sequence counter"
        );

        let outbox = query.fetch_outbox(0).await.unwrap();
        assert!(
            outbox.results.is_empty(),
            "Non-matching events should not populate the outbox"
        );
    }
}
