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
    use crate::test_support::helpers::test_fixtures::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    async fn create_test_manager() -> (Arc<ReactionManager>, mpsc::Receiver<ComponentEvent>) {
        let (event_tx, event_rx) = mpsc::channel(100);
        let manager = Arc::new(ReactionManager::new(event_tx));
        (manager, event_rx)
    }

    #[tokio::test]
    async fn test_add_reaction() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        let result = manager.add_reaction(config.clone()).await;

        assert!(result.is_ok());

        // Verify reaction was added
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, "test-reaction");
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec![]);

        // Add reaction first time
        assert!(manager.add_reaction(config.clone()).await.is_ok());

        // Try to add same reaction again
        let result = manager.add_reaction(config).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_delete_reaction() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec![]);
        manager.add_reaction(config).await.unwrap();

        // Delete the reaction
        let result = manager.delete_reaction("test-reaction".to_string()).await;
        assert!(result.is_ok());

        // Verify reaction was removed
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_start_reaction() {
        let (manager, mut event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        manager.add_reaction(config).await.unwrap();

        // Create a channel for query results (normally provided by router)
        let (_tx, rx) = mpsc::channel(100);

        let result = manager
            .start_reaction("test-reaction".to_string(), rx)
            .await;
        assert!(result.is_ok());

        // Check for status event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-reaction" {
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
    async fn test_stop_reaction() {
        let (manager, mut event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec![]);
        manager.add_reaction(config).await.unwrap();

        // Create a channel for query results
        let (_tx, rx) = mpsc::channel(100);

        // Start and then stop the reaction
        manager
            .start_reaction("test-reaction".to_string(), rx)
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let result = manager.stop_reaction("test-reaction".to_string()).await;
        assert!(result.is_ok());

        // Check for stop event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-reaction"
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
    async fn test_get_reaction_config() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        manager.add_reaction(config.clone()).await.unwrap();

        let retrieved = manager.get_reaction_config("test-reaction").await;
        assert!(retrieved.is_some());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, config.id);
        assert_eq!(retrieved.reaction_type, config.reaction_type);
        assert_eq!(retrieved.queries, config.queries);
    }

    #[tokio::test]
    async fn test_update_reaction() {
        let (manager, _event_rx) = create_test_manager().await;

        let mut config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        manager.add_reaction(config.clone()).await.unwrap();

        // Update config
        config.queries.push("query2".to_string());

        let result = manager
            .update_reaction("test-reaction".to_string(), config.clone())
            .await;
        assert!(result.is_ok());

        // Verify update
        let retrieved = manager.get_reaction_config("test-reaction").await.unwrap();
        assert_eq!(retrieved.queries.len(), 2);
        assert!(retrieved.queries.contains(&"query2".to_string()));
    }

    #[tokio::test]
    async fn test_reaction_lifecycle() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        manager.add_reaction(config).await.unwrap();

        // Verify reaction is in stopped state
        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Create a channel for query results
        let (_tx, rx) = mpsc::channel(100);

        // Start the reaction
        manager
            .start_reaction("test-reaction".to_string(), rx)
            .await
            .unwrap();

        // Verify reaction is running
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Running));

        // Stop the reaction
        manager
            .stop_reaction("test-reaction".to_string())
            .await
            .unwrap();

        // Verify reaction is stopped
        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await
            .unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));
    }
}

#[cfg(test)]
mod log_reaction_tests {
    use super::super::{LogReaction, Reaction};
    use crate::channels::*;
    use crate::config::ReactionConfig;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_log_reaction_creation() {
        let (event_tx, _event_rx) = mpsc::channel(100);

        let mut properties = HashMap::new();
        properties.insert("log_level".to_string(), json!("info"));

        let config = ReactionConfig {
            id: "test-log".to_string(),
            reaction_type: "log".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: false,
            properties,
        };

        let reaction = LogReaction::new(config.clone(), event_tx);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_processes_results() {
        let (event_tx, _event_rx) = mpsc::channel(100);
        let (query_tx, query_rx) = mpsc::channel(100);

        let config = ReactionConfig {
            id: "test-log".to_string(),
            reaction_type: "log".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: false,
            properties: HashMap::new(),
        };

        let reaction = LogReaction::new(config, event_tx);

        // Start the reaction
        reaction.start(query_rx).await.unwrap();

        // Send a query result
        let result = QueryResult {
            query_id: "query1".to_string(),
            timestamp: chrono::Utc::now(),
            results: vec![json!({"test": "data"})],
            metadata: HashMap::new(),
        };

        query_tx.send(result).await.unwrap();

        // Give it time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Stop the reaction
        reaction.stop().await.unwrap();
    }
}
