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
    use crate::config::QueryConfig;
    use crate::queries::Query;
    use crate::server_core::DrasiServerCore;
    use crate::test_support::helpers::test_fixtures::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, RwLock};

    /// Mock query for testing reactions
    struct MockQuery {
        config: QueryConfig,
        status: Arc<RwLock<ComponentStatus>>,
        broadcast_tx: QueryResultBroadcastSender,
    }

    impl MockQuery {
        fn new(query_id: &str) -> Self {
            let (broadcast_tx, _) = tokio::sync::broadcast::channel(1000);
            Self {
                config: QueryConfig {
                    id: query_id.to_string(),
                    query: "MATCH (n) RETURN n".to_string(),
                    query_language: crate::config::QueryLanguage::Cypher,
                    sources: vec![],
                    auto_start: false,
                    properties: HashMap::new(),
                    joins: None,
                    enable_bootstrap: false,
                    bootstrap_buffer_size: 10000,
                    priority_queue_capacity: None,
                    broadcast_channel_capacity: None,
                },
                status: Arc::new(RwLock::new(ComponentStatus::Running)),
                broadcast_tx,
            }
        }
    }

    #[async_trait]
    impl Query for MockQuery {
        async fn start(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Running;
            Ok(())
        }

        async fn stop(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Stopped;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status.read().await.clone()
        }

        fn get_config(&self) -> &QueryConfig {
            &self.config
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn subscribe(
            &self,
            reaction_id: String,
        ) -> Result<QuerySubscriptionResponse, String> {
            let broadcast_receiver = self.broadcast_tx.subscribe();
            Ok(QuerySubscriptionResponse {
                query_id: self.config.id.clone(),
                reaction_id,
                broadcast_receiver,
            })
        }
    }

    async fn create_test_server_with_query(query_id: &str) -> Result<Arc<DrasiServerCore>> {
        let mock_query = Arc::new(MockQuery::new(query_id));
        let server_core = DrasiServerCore::builder().build().await?;
        server_core
            .query_manager()
            .add_query_instance_for_test(mock_query)
            .await?;
        Ok(Arc::new(server_core))
    }

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

        // Create test server with mock query
        let server_core = create_test_server_with_query("query1").await.unwrap();

        // Start the reaction
        let result = manager
            .start_reaction("test-reaction".to_string(), server_core)
            .await;
        assert!(result.is_ok(), "Should be able to start reaction");

        // Wait for status event
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

        let config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        manager.add_reaction(config).await.unwrap();

        // Create test server and start reaction
        let server_core = create_test_server_with_query("query1").await.unwrap();
        manager
            .start_reaction("test-reaction".to_string(), server_core)
            .await
            .unwrap();

        // Wait a bit for reaction to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Stop the reaction
        let result = manager.stop_reaction("test-reaction".to_string()).await;
        assert!(result.is_ok(), "Should be able to stop reaction");

        // Wait for stop event
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

        // Update config to add another query
        config.queries.push("query2".to_string());

        // Update with server_core (not started, so pass None)
        let result = manager
            .update_reaction("test-reaction".to_string(), config.clone(), None)
            .await;
        assert!(result.is_ok(), "Should be able to update reaction");

        // Verify config was updated
        let retrieved = manager.get_reaction_config("test-reaction").await.unwrap();
        assert_eq!(retrieved.queries.len(), 2);
        assert_eq!(retrieved.queries[0], "query1");
        assert_eq!(retrieved.queries[1], "query2");
    }

    #[tokio::test]
    async fn test_reaction_lifecycle() {
        let (manager, _event_rx) = create_test_manager().await;

        let config = create_test_reaction_config("test-reaction", vec!["query1".to_string()]);
        manager.add_reaction(config).await.unwrap();

        // 1. Verify reaction starts in stopped state
        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Reaction should start in stopped state"
        );

        // 2. Create server and start reaction
        let server_core = create_test_server_with_query("query1").await.unwrap();
        manager
            .start_reaction("test-reaction".to_string(), server_core)
            .await
            .unwrap();

        // 3. Verify reaction is running
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Starting)
                || matches!(status, ComponentStatus::Running),
            "Reaction should be starting or running"
        );

        // 4. Stop reaction
        manager
            .stop_reaction("test-reaction".to_string())
            .await
            .unwrap();

        // 5. Verify reaction is stopped
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Reaction should be stopped"
        );
    }
}

#[cfg(test)]
mod log_reaction_tests {
    use super::super::{LogReaction, Reaction};
    use crate::channels::*;
    use crate::config::{QueryConfig, ReactionConfig};
    use crate::queries::Query;
    use crate::server_core::DrasiServerCore;
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{mpsc, RwLock};

    /// Mock query for testing reactions
    struct MockQuery {
        config: QueryConfig,
        status: Arc<RwLock<ComponentStatus>>,
        broadcast_tx: QueryResultBroadcastSender,
    }

    impl MockQuery {
        fn new(query_id: &str) -> Self {
            let (broadcast_tx, _) = tokio::sync::broadcast::channel(1000);
            Self {
                config: QueryConfig {
                    id: query_id.to_string(),
                    query: "MATCH (n) RETURN n".to_string(),
                    query_language: crate::config::QueryLanguage::Cypher,
                    sources: vec![],
                    auto_start: false,
                    properties: HashMap::new(),
                    joins: None,
                    enable_bootstrap: false,
                    bootstrap_buffer_size: 10000,
                    priority_queue_capacity: None,
                    broadcast_channel_capacity: None,
                },
                status: Arc::new(RwLock::new(ComponentStatus::Running)),
                broadcast_tx,
            }
        }
    }

    #[async_trait]
    impl Query for MockQuery {
        async fn start(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Running;
            Ok(())
        }

        async fn stop(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Stopped;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status.read().await.clone()
        }

        fn get_config(&self) -> &QueryConfig {
            &self.config
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn subscribe(
            &self,
            reaction_id: String,
        ) -> Result<QuerySubscriptionResponse, String> {
            let broadcast_receiver = self.broadcast_tx.subscribe();
            Ok(QuerySubscriptionResponse {
                query_id: self.config.id.clone(),
                reaction_id,
                broadcast_receiver,
            })
        }
    }

    async fn create_test_server_with_query(query_id: &str) -> Result<Arc<DrasiServerCore>> {
        let mock_query = Arc::new(MockQuery::new(query_id));
        let server_core = DrasiServerCore::builder().build().await?;
        server_core
            .query_manager()
            .add_query_instance_for_test(mock_query)
            .await?;
        Ok(Arc::new(server_core))
    }

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
            priority_queue_capacity: None,
        };

        let reaction = LogReaction::new(config.clone(), event_tx);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_processes_results() {
        let (event_tx, _event_rx) = mpsc::channel(100);

        let config = ReactionConfig {
            id: "test-log".to_string(),
            reaction_type: "log".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: false,
            properties: HashMap::new(),
            priority_queue_capacity: None,
        };

        let reaction = LogReaction::new(config, event_tx);

        // Create test server with mock query
        let server_core = create_test_server_with_query("query1").await.unwrap();

        // Start the reaction (it will subscribe to the mock query)
        reaction.start(server_core).await.unwrap();

        // Verify reaction is running
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert_eq!(reaction.status().await, ComponentStatus::Running);

        // Stop the reaction
        reaction.stop().await.unwrap();

        // Verify reaction is stopped
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }
}
