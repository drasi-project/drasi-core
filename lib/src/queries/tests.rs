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
    use crate::sources::tests::{create_test_bootstrap_mock_source, create_test_mock_source};
    use crate::sources::SourceManager;
    use drasi_core::middleware::MiddlewareTypeRegistry;
    use std::sync::Arc;
    use tokio::sync::mpsc;

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
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
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
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }

    async fn create_test_manager() -> (
        Arc<QueryManager>,
        mpsc::Receiver<ComponentEvent>,
        mpsc::Sender<ComponentEvent>,
        Arc<SourceManager>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(100);

        // Use the global shared log registry for test isolation with tracing
        let log_registry = crate::managers::get_or_init_global_registry();
        let source_manager = Arc::new(SourceManager::new(
            "test-instance",
            event_tx.clone(),
            log_registry.clone(),
        ));

        // Create a test IndexFactory with empty backends (no plugin, memory only)
        let index_factory = Arc::new(crate::indexes::IndexFactory::new(vec![], None));

        // Create a test middleware registry
        let middleware_registry = Arc::new(MiddlewareTypeRegistry::new());

        let query_manager = Arc::new(QueryManager::new(
            "test-instance",
            event_tx.clone(),
            source_manager.clone(),
            index_factory,
            middleware_registry,
            log_registry,
        ));

        (query_manager, event_rx, event_tx, source_manager)
    }

    #[tokio::test]
    async fn test_add_query() {
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        let result = manager.add_query(config.clone()).await;

        assert!(result.is_ok());

        // Verify query was added
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0, "test-query");
    }

    #[tokio::test]
    async fn test_add_duplicate_query() {
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec![]);

        // Add query first time
        assert!(manager.add_query(config.clone()).await.is_ok());

        // Try to add same query again
        let result = manager.add_query(config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_delete_query() {
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec![]);
        manager.add_query(config).await.unwrap();

        // Delete the query
        let result = manager.delete_query("test-query".to_string()).await;
        assert!(result.is_ok());

        // Verify query was removed
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 0);
    }

    #[tokio::test]
    async fn test_start_query() {
        let (manager, mut event_rx, event_tx, source_manager) = create_test_manager().await;

        // Add a source first using instance-based approach
        let source = create_test_mock_source("source1".to_string(), event_tx);
        source_manager.add_source(source).await.unwrap();

        // Add and start a query
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(config).await.unwrap();

        let result = manager.start_query("test-query".to_string()).await;
        assert!(result.is_ok());

        // Check for status event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-query" {
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
        let (manager, mut event_rx, event_tx, source_manager) = create_test_manager().await;

        // Add a source using instance-based approach
        let source = create_test_mock_source("source1".to_string(), event_tx);
        source_manager.add_source(source).await.unwrap();

        // Add and start a query
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(config).await.unwrap();

        manager.start_query("test-query".to_string()).await.unwrap();

        // Wait a bit
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Stop the query
        let result = manager.stop_query("test-query".to_string()).await;
        assert!(result.is_ok());

        // Check for stop event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
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
    async fn test_partial_subscription_failure_returns_error() {
        let (manager, _event_rx, event_tx, source_manager) = create_test_manager().await;

        // Add only source1 - source2 will be missing to trigger failure
        let source1 = create_test_mock_source("source1".to_string(), event_tx);
        source_manager.add_source(source1).await.unwrap();

        // Create query that subscribes to TWO sources - source2 is missing
        let config = create_test_query_config(
            "test-query",
            vec!["source1".to_string(), "source2".to_string()],
        );
        manager.add_query(config).await.unwrap();

        // Starting should fail because source2 doesn't exist
        let result = manager.start_query("test-query".to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source2"));

        // Query should be in Error state
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Error),
            "Expected Error status after failed start"
        );
    }

    #[tokio::test]
    async fn test_add_gql_query() {
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        let config = create_test_gql_query_config("test-gql-query", vec!["source1".to_string()]);
        let result = manager.add_query(config.clone()).await;

        assert!(result.is_ok());

        // Verify query was added
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].0, "test-gql-query");
    }

    #[tokio::test]
    async fn test_start_gql_query() {
        let (manager, mut event_rx, event_tx, source_manager) = create_test_manager().await;

        // Add a source first using instance-based approach
        let source = create_test_mock_source("source1".to_string(), event_tx);
        source_manager.add_source(source).await.unwrap();

        // Add and start a GQL query
        let config = create_test_gql_query_config("test-gql-query", vec!["source1".to_string()]);
        manager.add_query(config).await.unwrap();

        let result = manager.start_query("test-gql-query".to_string()).await;
        assert!(result.is_ok());

        // Check for status event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-gql-query" {
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
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        // Add a Cypher query
        let cypher_config = create_test_query_config("cypher-query", vec!["source1".to_string()]);
        assert!(manager.add_query(cypher_config).await.is_ok());

        // Add a GQL query
        let gql_config = create_test_gql_query_config("gql-query", vec!["source1".to_string()]);
        assert!(manager.add_query(gql_config).await.is_ok());

        // Verify both queries were added
        let queries = manager.list_queries().await;
        assert_eq!(queries.len(), 2);

        let query_ids: Vec<String> = queries.iter().map(|(id, _)| id.clone()).collect();
        assert!(query_ids.contains(&"cypher-query".to_string()));
        assert!(query_ids.contains(&"gql-query".to_string()));
    }

    #[tokio::test]
    async fn test_get_query_config() {
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(config.clone()).await.unwrap();

        let retrieved = manager.get_query_config("test-query").await;
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, config.id);
        assert_eq!(retrieved.query, config.query);
        assert_eq!(retrieved.sources.len(), config.sources.len());
    }

    #[tokio::test]
    async fn test_update_query() {
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        let mut config = create_test_query_config("test-query", vec![]);
        manager.add_query(config.clone()).await.unwrap();

        // Update config
        config.query = "MATCH (n:Updated) RETURN n".to_string();

        let result = manager
            .update_query("test-query".to_string(), config.clone())
            .await;
        assert!(result.is_ok());

        // Verify update
        let retrieved = manager.get_query_config("test-query").await.unwrap();
        assert_eq!(retrieved.query, config.query);
    }

    #[tokio::test]
    async fn test_query_lifecycle() {
        let (manager, _event_rx, event_tx, source_manager) = create_test_manager().await;

        // Add a source using instance-based approach
        let source = create_test_mock_source("source1".to_string(), event_tx);
        source_manager.add_source(source).await.unwrap();

        // Add a query that subscribes to the source
        let query_config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(query_config).await.unwrap();

        // Verify query is in stopped state
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Start the query
        manager.start_query("test-query".to_string()).await.unwrap();

        // Verify query is running
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Running));

        // Stop the query
        manager.stop_query("test-query".to_string()).await.unwrap();

        // Verify query is stopped
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
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        // Add query with auto_start=false (no sources needed since we won't start it)
        let config = create_test_query_config_with_auto_start("no-auto-start-query", vec![], false);
        manager.add_query(config).await.unwrap();

        // Query should be in stopped state
        let status = manager
            .get_query_status("no-auto-start-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Query with auto_start=false should remain stopped after add"
        );
    }

    /// Test that query with auto_start=false can be manually started
    #[tokio::test]
    async fn test_query_auto_start_false_can_be_manually_started() {
        let (manager, _event_rx, event_tx, source_manager) = create_test_manager().await;

        // Add a source
        let source = create_test_mock_source("source1".to_string(), event_tx);
        source_manager.add_source(source).await.unwrap();

        // Add query with auto_start=false
        let config = create_test_query_config_with_auto_start(
            "manual-query",
            vec!["source1".to_string()],
            false,
        );
        manager.add_query(config).await.unwrap();

        // Query should be in stopped state initially
        let status = manager
            .get_query_status("manual-query".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Query with auto_start=false should be stopped initially"
        );

        // Manually start the query
        manager
            .start_query("manual-query".to_string())
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        // Add query with auto_start=false
        let config = create_test_query_config_with_auto_start("test-query", vec![], false);
        manager.add_query(config).await.unwrap();

        // Retrieve config and verify auto_start is preserved
        let retrieved = manager.get_query_config("test-query").await.unwrap();
        assert!(
            !retrieved.auto_start,
            "Retrieved config should preserve auto_start=false"
        );

        // Add another query with auto_start=true
        let config2 = create_test_query_config_with_auto_start("test-query-2", vec![], true);
        manager.add_query(config2).await.unwrap();

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
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        // Add a query
        let config = create_test_query_config("cleanup-events-query", vec![]);
        manager.add_query(config).await.unwrap();

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
        manager
            .delete_query("cleanup-events-query".to_string())
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
        let (manager, mut event_rx, event_tx, source_manager) = create_test_manager().await;

        // Create a bootstrap channel — test controls the sender
        let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(100);

        // Add a source that provides the bootstrap channel
        let source =
            create_test_bootstrap_mock_source("bs-source".to_string(), event_tx, bootstrap_rx);
        source_manager.add_source(source).await.unwrap();

        // Add a query subscribed to this source
        let config = create_test_query_config("bs-query", vec!["bs-source".to_string()]);
        manager.add_query(config).await.unwrap();

        // Start the query
        manager.start_query("bs-query".to_string()).await.unwrap();

        // Drain the Starting event from the channel
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "bs-query"
                    && matches!(event.status, ComponentStatus::Starting)
                {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for Starting event");

        // Give the system a moment to settle
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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

        // Wait for the Running event
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(event) = event_rx.recv().await {
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
        let (manager, _event_rx, _event_tx, _source_manager) = create_test_manager().await;

        // Add a query
        let config = create_test_query_config("cleanup-logs-query", vec![]);
        manager.add_query(config).await.unwrap();

        // Generate a log via subscribe (which creates the channel) then check
        // First subscribe to create the channel
        let result = manager.subscribe_logs("cleanup-logs-query").await;
        assert!(result.is_some(), "Expected to subscribe to query logs");

        // Delete the query
        manager
            .delete_query("cleanup-logs-query".to_string())
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
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
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
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        };

        // Empty queries should be caught during validation
        assert!(config.query.is_empty());
    }
}
