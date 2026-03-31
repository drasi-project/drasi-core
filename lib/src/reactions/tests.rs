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
pub(crate) mod manager_tests {
    use super::super::*;
    use crate::channels::*;
    use crate::test_helpers::wait_for_component_status;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::RwLock;

    struct MockQueryProvider;

    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn get_query_instance(
            &self,
            id: &str,
        ) -> anyhow::Result<Arc<dyn crate::queries::Query>> {
            Err(anyhow::anyhow!("No query '{id}' in test"))
        }
    }

    /// A simple test mock reaction for unit testing the ReactionManager.
    pub struct TestMockReaction {
        id: String,
        queries: Vec<String>,
        auto_start: bool,
        /// Status handle — always available, wired to graph during initialize().
        status_handle: crate::component_graph::ComponentStatusHandle,
    }

    impl TestMockReaction {
        pub fn new(id: String, queries: Vec<String>) -> Self {
            let status_handle = crate::component_graph::ComponentStatusHandle::new(&id);
            Self {
                id,
                queries,
                auto_start: true,
                status_handle,
            }
        }

        pub fn with_auto_start(id: String, queries: Vec<String>, auto_start: bool) -> Self {
            let status_handle = crate::component_graph::ComponentStatusHandle::new(&id);
            Self {
                id,
                queries,
                auto_start,
                status_handle,
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

        async fn initialize(&self, context: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }

        async fn start(&self) -> anyhow::Result<()> {
            self.status_handle
                .set_status(
                    ComponentStatus::Starting,
                    Some("Starting reaction".to_string()),
                )
                .await;
            self.status_handle
                .set_status(
                    ComponentStatus::Running,
                    Some("Reaction started".to_string()),
                )
                .await;
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            self.status_handle
                .set_status(
                    ComponentStatus::Stopping,
                    Some("Stopping reaction".to_string()),
                )
                .await;
            self.status_handle
                .set_status(
                    ComponentStatus::Stopped,
                    Some("Reaction stopped".to_string()),
                )
                .await;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }

        async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> {
            Ok(())
        }
    }

    /// Helper to create a TestMockReaction instance
    pub fn create_test_mock_reaction(id: String, queries: Vec<String>) -> TestMockReaction {
        TestMockReaction::new(id, queries)
    }

    async fn create_test_manager() -> (
        Arc<ReactionManager>,
        Arc<tokio::sync::RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        // Use the global shared log registry for test isolation with tracing
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

        let manager = Arc::new(ReactionManager::new(
            "test-instance",
            log_registry,
            graph.clone(),
            update_tx,
        ));
        // Inject mock QueryProvider so add_reaction() can construct ReactionRuntimeContext
        manager
            .inject_query_provider(Arc::new(MockQueryProvider))
            .await;
        (manager, graph)
    }

    /// Create a test manager that also returns the shared graph for event subscription.
    async fn create_test_manager_with_graph() -> (
        Arc<ReactionManager>,
        Arc<tokio::sync::RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        create_test_manager().await
    }

    /// Helper: register a reaction in the graph, then provision it in the manager.
    /// Registers placeholder query nodes for any referenced queries not already in the graph.
    async fn add_reaction(
        manager: &ReactionManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        reaction: impl crate::reactions::Reaction + 'static,
    ) -> anyhow::Result<()> {
        let reaction_id = reaction.id().to_string();
        let reaction_type = reaction.type_name().to_string();
        let query_ids = reaction.query_ids();
        {
            let mut g = graph.write().await;
            // Ensure referenced queries exist as placeholder nodes
            for qid in &query_ids {
                if !g.contains(qid) {
                    g.register_query(qid, HashMap::new(), &[])?;
                }
            }
            let mut metadata = HashMap::new();
            metadata.insert("kind".to_string(), reaction_type);
            g.register_reaction(&reaction_id, metadata, &query_ids)?;
        }
        manager.provision_reaction(reaction).await
    }

    /// Helper: teardown a reaction in the manager, then deregister from the graph.
    async fn delete_reaction(
        manager: &ReactionManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        id: &str,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        manager.teardown_reaction(id.to_string(), cleanup).await?;
        let mut g = graph.write().await;
        g.deregister(id)?;
        Ok(())
    }

    #[tokio::test]
    async fn test_add_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction =
            create_test_mock_reaction("test-reaction".to_string(), vec!["query1".to_string()]);
        let result = add_reaction(&manager, &graph, reaction).await;

        assert!(result.is_ok());

        // Verify reaction was added
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].0, "test-reaction");
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction1 = create_test_mock_reaction("test-reaction".to_string(), vec![]);
        let reaction2 = create_test_mock_reaction("test-reaction".to_string(), vec![]);

        // Add reaction first time
        assert!(add_reaction(&manager, &graph, reaction1).await.is_ok());

        // Try to add same reaction again
        let result = add_reaction(&manager, &graph, reaction2).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_delete_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction = create_test_mock_reaction("test-reaction".to_string(), vec![]);
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Delete the reaction
        let result = delete_reaction(&manager, &graph, "test-reaction", false).await;
        assert!(result.is_ok());

        // Verify reaction was removed
        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_reaction() {
        let (manager, graph) = create_test_manager().await;

        let result = delete_reaction(&manager, &graph, "nonexistent", false).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_get_reaction_info() {
        let (manager, graph) = create_test_manager().await;

        let reaction =
            create_test_mock_reaction("test-reaction".to_string(), vec!["query1".to_string()]);
        add_reaction(&manager, &graph, reaction).await.unwrap();

        let retrieved = manager.get_reaction("test-reaction".to_string()).await;
        assert!(retrieved.is_ok());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "test-reaction");
        assert_eq!(retrieved.reaction_type, "log");
        assert_eq!(retrieved.queries, vec!["query1".to_string()]);
    }

    #[tokio::test]
    async fn test_list_reactions_with_status() {
        let (manager, graph) = create_test_manager().await;

        // Add multiple reactions
        let reaction1 = create_test_mock_reaction("reaction1".to_string(), vec![]);
        let reaction2 = create_test_mock_reaction("reaction2".to_string(), vec![]);

        add_reaction(&manager, &graph, reaction1).await.unwrap();
        add_reaction(&manager, &graph, reaction2).await.unwrap();

        let reactions = manager.list_reactions().await;
        assert_eq!(reactions.len(), 2);

        // Both should be stopped initially
        for (_, status) in reactions {
            assert!(matches!(status, ComponentStatus::Stopped));
        }
    }

    #[tokio::test]
    async fn test_get_reaction_status() {
        let (manager, graph) = create_test_manager().await;

        let reaction = create_test_mock_reaction("test-reaction".to_string(), vec![]);
        add_reaction(&manager, &graph, reaction).await.unwrap();

        let status = manager
            .get_reaction_status("test-reaction".to_string())
            .await;
        assert!(status.is_ok());
        assert!(matches!(status.unwrap(), ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_get_nonexistent_reaction_status() {
        let (manager, graph) = create_test_manager().await;

        let status = manager.get_reaction_status("nonexistent".to_string()).await;
        assert!(status.is_err());
        assert!(status.unwrap_err().to_string().contains("not found"));
    }

    /// Test that concurrent add_reaction calls with the same ID are handled atomically.
    /// Only one should succeed, the others should fail with "already exists".
    /// This tests the TOCTOU fix where we use a single write lock for check-and-insert.
    #[tokio::test]
    async fn test_concurrent_add_reaction_same_id() {
        let (manager, graph) = create_test_manager().await;

        // Spawn multiple tasks trying to add a reaction with the same ID concurrently
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let graph_clone = graph.clone();
            handles.push(tokio::spawn(async move {
                let reaction = create_test_mock_reaction("same-reaction".to_string(), vec![]);
                let result = add_reaction(&manager_clone, &graph_clone, reaction).await;
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
        let (manager, graph) = create_test_manager().await;

        // Spawn multiple tasks adding reactions with unique IDs
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let graph_clone = graph.clone();
            handles.push(tokio::spawn(async move {
                let reaction = create_test_mock_reaction(format!("reaction-{i}"), vec![]);
                add_reaction(&manager_clone, &graph_clone, reaction).await
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
        let (manager, graph) = create_test_manager().await;

        // Add reaction with auto_start=true
        let reaction1 =
            TestMockReaction::with_auto_start("auto-start-reaction".to_string(), vec![], true);
        add_reaction(&manager, &graph, reaction1).await.unwrap();

        // Add reaction with auto_start=false
        let reaction2 =
            TestMockReaction::with_auto_start("no-auto-start-reaction".to_string(), vec![], false);
        add_reaction(&manager, &graph, reaction2).await.unwrap();

        // Subscribe BEFORE the operation that triggers status changes
        let mut event_rx = graph.read().await.subscribe();

        // Start all reactions
        manager.start_all().await.unwrap();

        // Wait for the auto-start reaction to reach Running
        wait_for_component_status(
            &mut event_rx,
            "auto-start-reaction",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

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
        let (manager, graph) = create_test_manager().await;

        // Create reaction using default constructor (should have auto_start=true)
        let reaction = create_test_mock_reaction("default-reaction".to_string(), vec![]);

        // Verify auto_start is true
        use crate::reactions::Reaction;
        assert!(reaction.auto_start(), "Default auto_start should be true");

        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Subscribe BEFORE the operation that triggers status changes
        let mut event_rx = graph.read().await.subscribe();

        // Start all should start this reaction
        manager.start_all().await.unwrap();

        // Wait for the reaction to reach Running
        wait_for_component_status(
            &mut event_rx,
            "default-reaction",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

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
        let (manager, graph) = create_test_manager().await;

        // Add reaction with auto_start=false
        let reaction =
            TestMockReaction::with_auto_start("manual-reaction".to_string(), vec![], false);
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // start_all should not start it
        manager.start_all().await.unwrap();

        // No status change expected for auto_start=false; yield to let any queued work complete
        tokio::task::yield_now().await;

        let status = manager
            .get_reaction_status("manual-reaction".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Reaction with auto_start=false should not be started by start_all"
        );

        // Subscribe BEFORE the operation that triggers status change
        let mut event_rx = graph.read().await.subscribe();

        // Manually start the reaction
        manager
            .start_reaction("manual-reaction".to_string())
            .await
            .unwrap();

        // Wait for the reaction to reach Running
        wait_for_component_status(
            &mut event_rx,
            "manual-reaction",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

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
        let (manager, graph) = create_test_manager().await;

        // Add a reaction
        let reaction = create_test_mock_reaction("cleanup-events-reaction".to_string(), vec![]);
        add_reaction(&manager, &graph, reaction).await.unwrap();

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
        delete_reaction(&manager, &graph, "cleanup-events-reaction", false)
            .await
            .unwrap();

        // Verify events are cleaned up
        let events_after = manager.get_reaction_events("cleanup-events-reaction").await;
        assert!(events_after.is_empty(), "Expected no events after deletion");
    }

    /// Test that deleting a reaction cleans up its log history
    #[tokio::test]
    async fn test_delete_reaction_cleans_up_log_history() {
        let (manager, graph) = create_test_manager().await;

        // Add a reaction
        let reaction = create_test_mock_reaction("cleanup-logs-reaction".to_string(), vec![]);
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Subscribe to create the channel
        let result = manager.subscribe_logs("cleanup-logs-reaction").await;
        assert!(result.is_some(), "Expected to subscribe to reaction logs");

        // Delete the reaction
        delete_reaction(&manager, &graph, "cleanup-logs-reaction", false)
            .await
            .unwrap();

        // Verify logs are cleaned up (subscribe should fail for non-existent reaction)
        let result = manager.subscribe_logs("cleanup-logs-reaction").await;
        assert!(result.is_none(), "Expected None for deleted reaction logs");
    }

    // ========================================================================
    // Deprovision tests
    // ========================================================================

    /// A test reaction that tracks deprovision calls.
    struct DeprovisionTestReaction {
        id: String,
        queries: Vec<String>,
        status_handle: crate::component_graph::ComponentStatusHandle,
        deprovision_called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl DeprovisionTestReaction {
        fn new(id: &str) -> (Self, Arc<std::sync::atomic::AtomicBool>) {
            let deprovision_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                Self {
                    id: id.to_string(),
                    queries: vec![],
                    status_handle: crate::component_graph::ComponentStatusHandle::new(id),
                    deprovision_called: deprovision_called.clone(),
                },
                deprovision_called,
            )
        }

        fn new_simple(id: &str) -> Self {
            Self {
                id: id.to_string(),
                queries: vec![],
                status_handle: crate::component_graph::ComponentStatusHandle::new(id),
                deprovision_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl crate::reactions::Reaction for DeprovisionTestReaction {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "deprovision-test"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }

        fn auto_start(&self) -> bool {
            false
        }

        async fn initialize(&self, context: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }

        async fn start(&self) -> anyhow::Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, None)
                .await;
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, None)
                .await;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }

        async fn deprovision(&self) -> anyhow::Result<()> {
            self.deprovision_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_delete_reaction_with_cleanup_calls_deprovision() {
        let (manager, graph) = create_test_manager().await;

        let (reaction, deprovision_flag) = DeprovisionTestReaction::new("deprovision-reaction");
        add_reaction(&manager, &graph, reaction).await.unwrap();

        delete_reaction(&manager, &graph, "deprovision-reaction", true)
            .await
            .unwrap();

        assert!(
            deprovision_flag.load(std::sync::atomic::Ordering::SeqCst),
            "deprovision() should have been called"
        );
    }

    #[tokio::test]
    async fn test_delete_reaction_without_cleanup_skips_deprovision() {
        let (manager, graph) = create_test_manager().await;

        let (reaction, deprovision_flag) = DeprovisionTestReaction::new("no-deprovision-reaction");
        add_reaction(&manager, &graph, reaction).await.unwrap();

        delete_reaction(&manager, &graph, "no-deprovision-reaction", false)
            .await
            .unwrap();

        assert!(
            !deprovision_flag.load(std::sync::atomic::Ordering::SeqCst),
            "deprovision() should NOT have been called"
        );
    }

    // ========================================================================
    // Update (replace instance) tests
    // ========================================================================

    #[tokio::test]
    async fn test_update_reaction_replaces_stopped_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction = DeprovisionTestReaction::new_simple("reconfig-stopped-reaction");
        add_reaction(&manager, &graph, reaction).await.unwrap();

        let new_reaction = DeprovisionTestReaction::new_simple("reconfig-stopped-reaction");
        manager
            .update_reaction("reconfig-stopped-reaction".to_string(), new_reaction)
            .await
            .unwrap();

        let status = manager
            .get_reaction_status("reconfig-stopped-reaction".to_string())
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_update_reaction_stops_and_restarts_running_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction = DeprovisionTestReaction::new_simple("reconfig-running-reaction");
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Subscribe BEFORE the operation that triggers status change
        let mut event_rx = graph.read().await.subscribe();

        manager
            .start_reaction("reconfig-running-reaction".to_string())
            .await
            .unwrap();
        // Wait for async graph update to propagate
        wait_for_component_status(
            &mut event_rx,
            "reconfig-running-reaction",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;
        let status = manager
            .get_reaction_status("reconfig-running-reaction".to_string())
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Running);

        let new_reaction = DeprovisionTestReaction::new_simple("reconfig-running-reaction");
        manager
            .update_reaction("reconfig-running-reaction".to_string(), new_reaction)
            .await
            .unwrap();

        // Wait for async graph update to propagate
        wait_for_component_status(
            &mut event_rx,
            "reconfig-running-reaction",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;
        let status = manager
            .get_reaction_status("reconfig-running-reaction".to_string())
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_update_reaction_emits_reconfiguring_event() {
        let (manager, graph) = create_test_manager_with_graph().await;

        // Subscribe to graph events BEFORE adding reaction
        let mut event_rx = graph.read().await.subscribe();

        let reaction = DeprovisionTestReaction::new_simple("reconfig-event-reaction");
        add_reaction(&manager, &graph, reaction).await.unwrap();

        let new_reaction = DeprovisionTestReaction::new_simple("reconfig-event-reaction");
        manager
            .update_reaction("reconfig-event-reaction".to_string(), new_reaction)
            .await
            .unwrap();

        // Wait for the Reconfiguring event via the broadcast channel
        wait_for_component_status(
            &mut event_rx,
            "reconfig-event-reaction",
            ComponentStatus::Reconfiguring,
            Duration::from_secs(5),
        )
        .await;
    }

    #[tokio::test]
    async fn test_update_reaction_rejects_mismatched_id() {
        let (manager, graph) = create_test_manager().await;

        let reaction = DeprovisionTestReaction::new_simple("original-reaction");
        add_reaction(&manager, &graph, reaction).await.unwrap();

        let new_reaction = DeprovisionTestReaction::new_simple("different-id");
        let result = manager
            .update_reaction("original-reaction".to_string(), new_reaction)
            .await;
        assert!(result.is_err(), "Expected error for mismatched IDs");
        assert!(result.unwrap_err().to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn test_update_nonexistent_reaction() {
        let (manager, graph) = create_test_manager().await;

        let new_reaction = DeprovisionTestReaction::new_simple("nonexistent");
        let result = manager
            .update_reaction("nonexistent".to_string(), new_reaction)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
