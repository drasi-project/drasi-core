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
    use crate::queries::Query;
    use crate::reactions::QueryProvider;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio::sync::RwLock;

    // Mock QueryProvider for testing ReactionManager
    struct MockQueryProvider;

    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn get_query_instance(&self, _id: &str) -> Result<Arc<dyn Query>> {
            Err(anyhow::anyhow!("MockQueryProvider: query not found"))
        }
    }

    /// A simple test mock reaction for unit testing the ReactionManager.
    struct TestMockReaction {
        id: String,
        queries: Vec<String>,
        auto_start: bool,
        status: Arc<RwLock<ComponentStatus>>,
        event_tx: ComponentEventSender,
    }

    impl TestMockReaction {
        fn new(id: String, queries: Vec<String>, event_tx: ComponentEventSender) -> Self {
            Self {
                id,
                queries,
                auto_start: true,
                status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
                event_tx,
            }
        }

        fn with_auto_start(
            id: String,
            queries: Vec<String>,
            event_tx: ComponentEventSender,
            auto_start: bool,
        ) -> Self {
            Self {
                id,
                queries,
                auto_start,
                status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
                event_tx,
            }
        }
    }

    #[async_trait]
    impl crate::reactions::Reaction for TestMockReaction {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "log"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }

        fn auto_start(&self) -> bool {
            self.auto_start
        }

        async fn initialize(&self, _context: crate::context::ReactionRuntimeContext) {
            // TestMockReaction already has event_tx from constructor
        }

        async fn start(&self) -> anyhow::Result<()> {
            *self.status.write().await = ComponentStatus::Starting;

            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Starting,
                timestamp: chrono::Utc::now(),
                message: Some("Starting reaction".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Running;

            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("Reaction started".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            *self.status.write().await = ComponentStatus::Stopping;

            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping reaction".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Stopped;

            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("Reaction stopped".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status.read().await.clone()
        }
    }

    /// Helper to create a TestMockReaction instance
    fn create_test_mock_reaction(
        id: String,
        queries: Vec<String>,
        event_tx: ComponentEventSender,
    ) -> TestMockReaction {
        TestMockReaction::new(id, queries, event_tx)
    }

    async fn create_test_manager() -> (
        Arc<ReactionManager>,
        mpsc::Receiver<ComponentEvent>,
        mpsc::Sender<ComponentEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(100);
        // Use the global shared log registry for test isolation with tracing
        let log_registry = crate::managers::get_or_init_global_registry();
        let manager = Arc::new(ReactionManager::new(
            "test-instance",
            event_tx.clone(),
            log_registry,
        ));
        // Inject mock QueryProvider so add_reaction() can construct ReactionRuntimeContext
        manager
            .inject_query_provider(Arc::new(MockQueryProvider))
            .await;
        (manager, event_rx, event_tx)
    }

    #[tokio::test]
    async fn test_add_reaction() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec!["query1".to_string()],
            event_tx,
        );
        let result = manager.add_reaction(reaction).await;

        assert!(result.is_ok());

        // Verify reaction was added
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, "test-reaction");
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction1 =
            create_test_mock_reaction("test-reaction".to_string(), vec![], event_tx.clone());
        let reaction2 = create_test_mock_reaction("test-reaction".to_string(), vec![], event_tx);

        // Add reaction first time
        assert!(manager.add_reaction(reaction1).await.is_ok());

        // Try to add same reaction again
        let result = manager.add_reaction(reaction2).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_delete_reaction() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction("test-reaction".to_string(), vec![], event_tx);
        manager.add_reaction(reaction).await.unwrap();

        // Delete the reaction
        let result = manager.delete_reaction("test-reaction".to_string()).await;
        assert!(result.is_ok());

        // Verify reaction was removed
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_reaction() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let result = manager.delete_reaction("nonexistent".to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_get_reaction_info() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            "test-reaction".to_string(),
            vec!["query1".to_string()],
            event_tx,
        );
        manager.add_reaction(reaction).await.unwrap();

        let retrieved = manager.get_reaction("test-reaction".to_string()).await;
        assert!(retrieved.is_ok());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "test-reaction");
        assert_eq!(retrieved.reaction_type, "log");
        assert_eq!(retrieved.queries, vec!["query1".to_string()]);
    }

    #[tokio::test]
    async fn test_list_reactions_with_status() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add multiple reactions
        let reaction1 =
            create_test_mock_reaction("reaction1".to_string(), vec![], event_tx.clone());
        let reaction2 = create_test_mock_reaction("reaction2".to_string(), vec![], event_tx);

        manager.add_reaction(reaction1).await.unwrap();
        manager.add_reaction(reaction2).await.unwrap();

        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 2);

        // Both should be stopped initially
        for (_, status) in reactions {
            assert!(matches!(status, ComponentStatus::Stopped));
        }
    }

    #[tokio::test]
    async fn test_get_reaction_status() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let reaction = create_test_mock_reaction("test-reaction".to_string(), vec![], event_tx);
        manager.add_reaction(reaction).await.unwrap();

        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await;
        assert!(status.is_ok());
        assert!(matches!(status.unwrap(), ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_get_nonexistent_reaction_status() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let status = manager.get_reaction_status("nonexistent".to_string()).await;
        assert!(status.is_err());
        assert!(status.unwrap_err().to_string().contains("not found"));
    }

    /// Test that concurrent add_reaction calls with the same ID are handled atomically.
    /// Only one should succeed, the others should fail with "already exists".
    /// This tests the TOCTOU fix where we use a single write lock for check-and-insert.
    #[tokio::test]
    async fn test_concurrent_add_reaction_same_id() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Spawn multiple tasks trying to add a reaction with the same ID concurrently
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let event_tx_clone = event_tx.clone();
            handles.push(tokio::spawn(async move {
                let reaction =
                    create_test_mock_reaction("same-reaction".to_string(), vec![], event_tx_clone);
                let result = manager_clone.add_reaction(reaction).await;
                (i, result.is_ok())
            }));
        }

        // Wait for all tasks to complete
        let mut success_count = 0;
        let mut failure_count = 0;
        for handle in handles {
            let (_i, succeeded) = handle.await.unwrap();
            if succeeded {
                success_count += 1;
            } else {
                failure_count += 1;
            }
        }

        // Exactly one should succeed, all others should fail
        assert_eq!(success_count, 1, "Exactly one add_reaction should succeed");
        assert_eq!(failure_count, 9, "All other add_reaction calls should fail");

        // Verify only one reaction exists
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, "same-reaction");
    }

    /// Test that concurrent add_reaction calls with different IDs all succeed.
    #[tokio::test]
    async fn test_concurrent_add_reaction_different_ids() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Spawn multiple tasks adding reactions with unique IDs
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let event_tx_clone = event_tx.clone();
            handles.push(tokio::spawn(async move {
                let reaction =
                    create_test_mock_reaction(format!("reaction-{i}"), vec![], event_tx_clone);
                manager_clone.add_reaction(reaction).await
            }));
        }

        // Wait for all tasks to complete
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(
                result.is_ok(),
                "All add_reaction calls with unique IDs should succeed"
            );
        }

        // Verify all 10 reactions exist
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 10);
    }

    // ============================================================================
    // Auto-start tests
    // ============================================================================

    /// Test that start_all only starts reactions with auto_start=true
    #[tokio::test]
    async fn test_start_all_respects_auto_start() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add reaction with auto_start=true
        let reaction1 = TestMockReaction::with_auto_start(
            "auto-start-reaction".to_string(),
            vec![],
            event_tx.clone(),
            true,
        );
        manager.add_reaction(reaction1).await.unwrap();

        // Add reaction with auto_start=false
        let reaction2 = TestMockReaction::with_auto_start(
            "no-auto-start-reaction".to_string(),
            vec![],
            event_tx.clone(),
            false,
        );
        manager.add_reaction(reaction2).await.unwrap();

        // Start all reactions
        manager.start_all().await.unwrap();

        // Wait a bit for status to update
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Check statuses
        let status1 = manager
            .get_reaction_status("auto-start-reaction".to_string())
            .await
            .unwrap();
        let status2 = manager
            .get_reaction_status("no-auto-start-reaction".to_string())
            .await
            .unwrap();

        assert!(
            matches!(status1, ComponentStatus::Running),
            "Reaction with auto_start=true should be running"
        );
        assert!(
            matches!(status2, ComponentStatus::Stopped),
            "Reaction with auto_start=false should still be stopped"
        );
    }

    /// Test that reaction auto_start defaults to true
    #[tokio::test]
    async fn test_reaction_auto_start_defaults_to_true() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Create reaction using default constructor (should have auto_start=true)
        let reaction = create_test_mock_reaction("default-reaction".to_string(), vec![], event_tx);

        // Verify auto_start is true
        use crate::reactions::Reaction;
        assert!(reaction.auto_start(), "Default auto_start should be true");

        manager.add_reaction(reaction).await.unwrap();

        // Start all should start this reaction
        manager.start_all().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let status = manager
            .get_reaction_status("default-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Running),
            "Default reaction should be started by start_all"
        );
    }

    /// Test that reaction with auto_start=false can be manually started
    #[tokio::test]
    async fn test_reaction_auto_start_false_can_be_manually_started() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add reaction with auto_start=false
        let reaction = TestMockReaction::with_auto_start(
            "manual-reaction".to_string(),
            vec![],
            event_tx,
            false,
        );
        manager.add_reaction(reaction).await.unwrap();

        // start_all should not start it
        manager.start_all().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let status = manager
            .get_reaction_status("manual-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Reaction with auto_start=false should not be started by start_all"
        );

        // Manually start the reaction
        manager
            .start_reaction("manual-reaction".to_string())
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let status = manager
            .get_reaction_status("manual-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Running),
            "Reaction with auto_start=false should be manually startable"
        );
    }

    // ============================================================================
    // Cleanup Tests
    // ============================================================================

    /// Test that deleting a reaction cleans up its event history
    #[tokio::test]
    async fn test_delete_reaction_cleans_up_event_history() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add a reaction
        let reaction =
            create_test_mock_reaction("cleanup-events-reaction".to_string(), vec![], event_tx);
        manager.add_reaction(reaction).await.unwrap();

        // Record an event manually to simulate lifecycle
        manager
            .record_event(ComponentEvent {
                component_id: "cleanup-events-reaction".to_string(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("Test event".to_string()),
            })
            .await;

        // Verify events exist
        let events = manager.get_reaction_events("cleanup-events-reaction").await;
        assert!(!events.is_empty(), "Expected events after recording");

        // Delete the reaction
        manager
            .delete_reaction("cleanup-events-reaction".to_string())
            .await
            .unwrap();

        // Verify events are cleaned up
        let events_after = manager.get_reaction_events("cleanup-events-reaction").await;
        assert!(events_after.is_empty(), "Expected no events after deletion");
    }

    /// Test that deleting a reaction cleans up its log history
    #[tokio::test]
    async fn test_delete_reaction_cleans_up_log_history() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add a reaction
        let reaction =
            create_test_mock_reaction("cleanup-logs-reaction".to_string(), vec![], event_tx);
        manager.add_reaction(reaction).await.unwrap();

        // Subscribe to create the channel
        let result = manager.subscribe_logs("cleanup-logs-reaction").await;
        assert!(result.is_some(), "Expected to subscribe to reaction logs");

        // Delete the reaction
        manager
            .delete_reaction("cleanup-logs-reaction".to_string())
            .await
            .unwrap();

        // Verify logs are cleaned up (subscribe should fail for non-existent reaction)
        let result = manager.subscribe_logs("cleanup-logs-reaction").await;
        assert!(result.is_none(), "Expected None for deleted reaction logs");
    }
}
