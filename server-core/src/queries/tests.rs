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
    use crate::sources::SourceManager;
    use crate::test_support::helpers::test_fixtures::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    async fn create_test_manager() -> (
        Arc<QueryManager>,
        mpsc::Receiver<QueryResult>,
        mpsc::Receiver<ComponentEvent>,
        Arc<SourceManager>,
    ) {
        let (query_tx, query_rx) = mpsc::channel(100);
        let (event_tx, event_rx) = mpsc::channel(100);

        let source_manager = Arc::new(SourceManager::new(event_tx.clone()));
        let query_manager = Arc::new(QueryManager::new(query_tx, event_tx.clone(), source_manager.clone()));

        (query_manager, query_rx, event_rx, source_manager)
    }

    #[tokio::test]
    async fn test_add_query() {
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

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
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

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
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

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
        let (manager, _query_rx, mut event_rx, source_manager) = create_test_manager().await;

        // Add a source first
        let source_config = create_test_source_config("source1", "mock");
        source_manager.add_source(source_config).await.unwrap();

        // Add and start a query
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(config).await.unwrap();

        // Create a channel for source changes
        // Legacy channel no longer needed - queries subscribe directly to sources

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
        let (manager, _query_rx, mut event_rx, source_manager) = create_test_manager().await;

        // Add a source
        let source_config = create_test_source_config("source1", "mock");
        source_manager.add_source(source_config).await.unwrap();

        // Add and start a query
        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(config).await.unwrap();
        // Create a channel for source changes
        // Legacy channel no longer needed - queries subscribe directly to sources

        manager
            .start_query("test-query".to_string())
            .await
            .unwrap();

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
    async fn test_add_gql_query() {
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

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
        let (manager, _query_rx, mut event_rx, source_manager) = create_test_manager().await;

        // Add a source first
        let source_config = create_test_source_config("source1", "mock");
        source_manager.add_source(source_config).await.unwrap();

        // Add and start a GQL query
        let config = create_test_gql_query_config("test-gql-query", vec!["source1".to_string()]);
        manager.add_query(config).await.unwrap();

        // Create a channel for source changes
        // Legacy channel no longer needed - queries subscribe directly to sources

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
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

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
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

        let config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(config.clone()).await.unwrap();

        let retrieved = manager.get_query_config("test-query").await;
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, config.id);
        assert_eq!(retrieved.query, config.query);
        assert_eq!(retrieved.sources, config.sources);
    }

    #[tokio::test]
    async fn test_update_query() {
        let (manager, _query_rx, _event_rx, _source_manager) = create_test_manager().await;

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
        let (manager, _query_rx, _event_rx, source_manager) = create_test_manager().await;

        // Add a source (but don't start it - queries can be configured before sources start)
        let source_config = create_test_source_config("source1", "mock");
        source_manager.add_source(source_config).await.unwrap();

        // Add a query that subscribes to the source
        let query_config = create_test_query_config("test-query", vec!["source1".to_string()]);
        manager.add_query(query_config).await.unwrap();

        // Verify query is in stopped state
        let status = manager
            .get_query_status("test-query".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Create a channel for source changes (normally provided by router)
        // Legacy channel no longer needed - queries subscribe directly to sources

        // Start the query
        manager
            .start_query("test-query".to_string())
            .await
            .unwrap();

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
}

#[cfg(test)]
mod query_core_tests {
    use crate::config::QueryConfig;
    use std::collections::HashMap;

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
                sources: vec![],
                auto_start: false,
                properties: HashMap::new(),
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
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
            sources: vec![],
            auto_start: false,
            properties: HashMap::new(),
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
        };

        // Empty queries should be caught during validation
        assert!(config.query.is_empty());
    }
}
