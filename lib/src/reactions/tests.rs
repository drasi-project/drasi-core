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
    use crate::channels::*;
    use crate::test_helpers::creation::BuilderFixtures;
    use crate::test_helpers::managers::ReactionManager;
    use crate::test_helpers::wait_for_component_status_by_id;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    /// A simple test mock reaction for unit testing the ReactionManager.
    pub struct TestMockReaction {
        id: drasi_core_types::ComponentId,
        queries: Vec<drasi_computation_contracts::ComponentReference>,
        auto_start: bool,
        /// Status handle — always available, wired to graph during initialize().
        status_handle: crate::component_graph::ComponentStatusHandle,
    }

    impl TestMockReaction {
        pub(crate) fn fixture(
            name: drasi_core_types::ComponentName,
            queries: Vec<crate::ComponentName>,
            auto_start: bool,
        ) -> crate::test_helpers::creation::ReactionFixture<Self> {
            let inputs: Vec<_> = queries
                .into_iter()
                .map(crate::config::ComponentReference::by_name)
                .collect();
            crate::test_helpers::creation::ReactionFixture::new(
                name,
                inputs,
                auto_start,
                move |id, resolved_inputs| {
                    Ok(Self::with_auto_start(
                        id,
                        resolved_inputs
                            .into_iter()
                            .map(crate::config::ComponentReference::by_id)
                            .collect(),
                        auto_start,
                    ))
                },
            )
        }

        pub fn with_auto_start(
            id: drasi_core_types::ComponentId,
            queries: Vec<drasi_computation_contracts::ComponentReference>,
            auto_start: bool,
        ) -> Self {
            let status_handle = crate::component_graph::ComponentStatusHandle::new(id.clone());
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
        fn id(&self) -> &drasi_core_types::ComponentId {
            &self.id
        }

        fn type_name(&self) -> &str {
            "log"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
            self.queries.clone()
        }

        fn auto_start(&self) -> bool {
            self.auto_start
        }

        async fn bind_runtime_context(&self, context: crate::context::ReactionRuntimeContext) {
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
    pub fn create_test_mock_reaction(
        name: drasi_core_types::ComponentName,
        queries: Vec<crate::ComponentName>,
    ) -> crate::test_helpers::creation::ReactionFixture<TestMockReaction> {
        TestMockReaction::fixture(name, queries, true)
    }

    async fn create_test_manager() -> (
        Arc<ReactionManager>,
        crate::component_graph::ComponentGraphView,
    ) {
        let core = crate::test_helpers::managers::empty_core().await;
        (
            Arc::new(ReactionManager(core.clone())),
            core.component_graph(),
        )
    }

    /// Helper: register a reaction in the graph, then provision it in the manager.
    /// Registers placeholder query nodes for any referenced queries not already in the graph.
    async fn add_reaction(
        manager: &ReactionManager,
        _graph: &crate::component_graph::ComponentGraphView,
        reaction: crate::test_helpers::creation::ReactionFixture<
            impl crate::reactions::Reaction + 'static,
        >,
    ) -> anyhow::Result<()> {
        manager.add(reaction).await
    }

    /// Helper: teardown a reaction in the manager, then deregister from the graph.
    async fn delete_reaction(
        manager: &ReactionManager,
        _graph: &crate::component_graph::ComponentGraphView,
        id: &drasi_core_types::ComponentName,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        manager.teardown_reaction_by_name(id, cleanup).await
    }

    #[tokio::test]
    async fn test_add_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            vec![
                drasi_core_types::ComponentName::try_new("query1").expect("fixture component name")
            ],
        );
        let result = add_reaction(&manager, &graph, reaction).await;

        assert!(result.is_ok());

        // Verify reaction was added
        let reactions = manager.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(
            reactions[0].name,
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name")
        );
    }

    #[tokio::test]
    async fn test_add_duplicate_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction1 = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            vec![],
        );
        let reaction2 = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            vec![],
        );

        // Add reaction first time
        assert!(add_reaction(&manager, &graph, reaction1).await.is_ok());
        let name = crate::ComponentName::try_new("test-reaction").unwrap();
        let admitted_id = manager.0.component_by_name(&name).unwrap().id().clone();

        // Try to add same reaction again
        let result = add_reaction(&manager, &graph, reaction2).await;
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(
            format!("{error:#}").contains("component name already belongs to 'test-reaction'"),
            "unexpected duplicate-reaction error: {error:#}"
        );
        assert_eq!(
            manager.0.component_by_name(&name).unwrap().id(),
            &admitted_id
        );
    }

    #[tokio::test]
    async fn test_delete_reaction() {
        let (manager, graph) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            vec![],
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Delete the reaction
        let result = delete_reaction(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            false,
        )
        .await;
        assert!(result.is_ok());

        // Verify reaction was removed
        let reactions = manager.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_reaction() {
        let (manager, graph) = create_test_manager().await;

        let result = delete_reaction(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("nonexistent")
                .expect("fixture component name"),
            false,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_get_reaction_info() {
        let (manager, graph) = create_test_manager().await;
        let query = manager
            .0
            .add_query_with_handle(
                crate::Query::cypher(crate::ComponentName::try_new("query1").unwrap())
                    .query("RETURN 1 AS value")
                    .auto_start(false)
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();

        let reaction = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            vec![
                drasi_core_types::ComponentName::try_new("query1").expect("fixture component name")
            ],
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();
        let reaction = manager
            .0
            .component_by_name(&crate::ComponentName::try_new("test-reaction").unwrap())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), reaction.wait_created())
            .await
            .unwrap()
            .unwrap();

        let retrieved = manager
            .get_reaction_by_name(
                &drasi_core_types::ComponentName::try_new("test-reaction")
                    .expect("fixture component name"),
            )
            .await;
        assert!(retrieved.is_ok());

        let retrieved = retrieved.unwrap();
        assert_eq!(
            &retrieved.id,
            manager.0.component_by_name(&retrieved.name).unwrap().id()
        );
        assert_eq!(retrieved.name.as_str(), "test-reaction");
        assert_eq!(retrieved.reaction_type, "log");
        assert_eq!(
            retrieved.queries,
            vec![crate::config::ComponentReference::by_id(query.id().clone())]
        );
        manager.0.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_list_reactions_with_status() {
        let (manager, graph) = create_test_manager().await;

        // Add multiple reactions
        let reaction1 = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("reaction1").expect("fixture component name"),
            vec![],
        );
        let reaction2 = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("reaction2").expect("fixture component name"),
            vec![],
        );

        add_reaction(&manager, &graph, reaction1).await.unwrap();
        add_reaction(&manager, &graph, reaction2).await.unwrap();

        let reactions = manager.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 2);

        // Both should be in Added state initially
        for entry in reactions {
            assert!(matches!(entry.status, ComponentStatus::Added));
        }
    }

    #[tokio::test]
    async fn test_get_reaction_status() {
        let (manager, graph) = create_test_manager().await;

        let reaction = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("test-reaction")
                .expect("fixture component name"),
            vec![],
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();

        let status = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("test-reaction")
                    .expect("fixture component name"),
            )
            .await;
        assert!(status.is_ok());
        assert!(matches!(status.unwrap(), ComponentStatus::Added));
    }

    #[tokio::test]
    async fn test_get_nonexistent_reaction_status() {
        let (manager, _graph) = create_test_manager().await;

        let status = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("nonexistent")
                    .expect("fixture component name"),
            )
            .await;
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
                let reaction = create_test_mock_reaction(
                    drasi_core_types::ComponentName::try_new("same-reaction")
                        .expect("fixture component name"),
                    vec![],
                );
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
        let reactions = manager.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(
            reactions[0].name,
            drasi_core_types::ComponentName::try_new("same-reaction")
                .expect("fixture component name")
        );
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
                let reaction = create_test_mock_reaction(
                    drasi_core_types::ComponentName::try_new(format!("reaction-{i}"))
                        .expect("fixture component name"),
                    vec![],
                );
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
        let reactions = manager.list_reactions().await.unwrap();
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
        let reaction1 = TestMockReaction::fixture(
            drasi_core_types::ComponentName::try_new("auto-start-reaction")
                .expect("fixture component name"),
            vec![],
            true,
        );
        add_reaction(&manager, &graph, reaction1).await.unwrap();

        // Add reaction with auto_start=false
        let reaction2 = TestMockReaction::fixture(
            drasi_core_types::ComponentName::try_new("no-auto-start-reaction")
                .expect("fixture component name"),
            vec![],
            false,
        );
        add_reaction(&manager, &graph, reaction2).await.unwrap();

        // Subscribe BEFORE the operation that triggers status changes
        let mut event_rx = graph.subscribe().unwrap();

        // Start all reactions
        manager.start_all().await.unwrap();

        // Wait for the auto-start reaction to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("auto-start-reaction").expect("fixture component name"),
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        // Check statuses
        let status1 = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("auto-start-reaction")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        let status2 = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("no-auto-start-reaction")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();

        assert!(
            matches!(status1, ComponentStatus::Running),
            "Reaction with auto_start=true should be running"
        );
        assert!(
            matches!(status2, ComponentStatus::Added),
            "Reaction with auto_start=false should still be in Added state"
        );
    }

    /// Test that reaction auto_start defaults to true
    #[tokio::test]
    async fn test_reaction_auto_start_defaults_to_true() {
        let (manager, graph) = create_test_manager().await;

        // Create reaction using default constructor (should have auto_start=true)
        let reaction = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("default-reaction")
                .expect("fixture component name"),
            vec![],
        );

        // Verify auto_start is true
        assert!(reaction.auto_start(), "Default auto_start should be true");

        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Subscribe BEFORE the operation that triggers status changes
        let mut event_rx = graph.subscribe().unwrap();

        // Start all should start this reaction
        manager.start_all().await.unwrap();

        // Wait for the reaction to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("default-reaction").expect("fixture component name"),
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        let status = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("default-reaction")
                    .expect("fixture component name"),
            )
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
        let reaction = TestMockReaction::fixture(
            drasi_core_types::ComponentName::try_new("manual-reaction")
                .expect("fixture component name"),
            vec![],
            false,
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // start_all should not start it
        manager.start_all().await.unwrap();

        // No status change expected for auto_start=false; yield to let any queued work complete
        tokio::task::yield_now().await;

        let status = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("manual-reaction")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Added),
            "Reaction with auto_start=false should not be started by start_all"
        );

        // Subscribe BEFORE the operation that triggers status change
        let mut event_rx = graph.subscribe().unwrap();

        // Manually start the reaction
        manager
            .start_reaction_by_name(
                &drasi_core_types::ComponentName::try_new("manual-reaction")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();

        // Wait for the reaction to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("manual-reaction").expect("fixture component name"),
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        let status = manager
            .get_reaction_status_by_name(
                &drasi_core_types::ComponentName::try_new("manual-reaction")
                    .expect("fixture component name"),
            )
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
        let reaction_name = drasi_core_types::ComponentName::try_new("cleanup-events-reaction")
            .expect("fixture component name");

        // Add a reaction
        let reaction = create_test_mock_reaction(reaction_name.clone(), vec![]);
        add_reaction(&manager, &graph, reaction).await.unwrap();
        let reaction_id = manager
            .0
            .component_by_name(&reaction_name)
            .unwrap()
            .id()
            .clone();

        manager
            .start_reaction_by_name(&reaction_name)
            .await
            .unwrap();

        // Verify events exist
        let events = manager
            .get_reaction_events_by_name(&reaction_name)
            .await
            .unwrap();
        assert!(!events.is_empty(), "Expected events after recording");

        // Delete the reaction
        delete_reaction(&manager, &graph, &reaction_name, false)
            .await
            .unwrap();

        // Verify events are cleaned up
        let events_after = manager
            .get_reaction_events_by_id(&reaction_id)
            .await
            .unwrap();
        assert!(events_after.is_empty(), "Expected no events after deletion");
    }

    /// Test that deleting a reaction cleans up its log history
    #[tokio::test]
    async fn test_delete_reaction_cleans_up_log_history() {
        let (manager, graph) = create_test_manager().await;

        // Add a reaction
        let reaction = create_test_mock_reaction(
            drasi_core_types::ComponentName::try_new("cleanup-logs-reaction")
                .expect("fixture component name"),
            vec![],
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();

        // Subscribe to create the channel
        let result = manager
            .subscribe_logs_by_name(
                &drasi_core_types::ComponentName::try_new("cleanup-logs-reaction")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_ok(), "Expected to subscribe to reaction logs");

        // Delete the reaction
        delete_reaction(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("cleanup-logs-reaction")
                .expect("fixture component name"),
            false,
        )
        .await
        .unwrap();

        // Verify logs are cleaned up (subscribe should fail for non-existent reaction)
        let result = manager
            .subscribe_logs_by_name(
                &drasi_core_types::ComponentName::try_new("cleanup-logs-reaction")
                    .expect("fixture component name"),
            )
            .await;
        assert!(
            result.is_err(),
            "Expected an error for deleted reaction logs"
        );
    }

    // ========================================================================
    // Deprovision tests
    // ========================================================================

    /// A test reaction that tracks deprovision calls.
    struct DeprovisionTestReaction {
        id: drasi_core_types::ComponentId,
        queries: Vec<drasi_computation_contracts::ComponentReference>,
        status_handle: crate::component_graph::ComponentStatusHandle,
        deprovision_called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl DeprovisionTestReaction {
        fn new(
            name: drasi_core_types::ComponentName,
        ) -> (
            crate::test_helpers::creation::ReactionFixture<Self>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            let deprovision_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let called = deprovision_called.clone();
            (
                crate::test_helpers::creation::ReactionFixture::new(
                    name,
                    vec![],
                    false,
                    move |id, _| {
                        Ok(Self {
                            id: id.clone(),
                            queries: vec![],
                            status_handle: crate::component_graph::ComponentStatusHandle::new(
                                id.clone(),
                            ),
                            deprovision_called: called,
                        })
                    },
                ),
                deprovision_called,
            )
        }
    }

    #[async_trait]
    impl crate::reactions::Reaction for DeprovisionTestReaction {
        fn id(&self) -> &drasi_core_types::ComponentId {
            &self.id
        }

        fn type_name(&self) -> &str {
            "deprovision-test"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
            self.queries.clone()
        }

        fn auto_start(&self) -> bool {
            false
        }

        async fn bind_runtime_context(&self, context: crate::context::ReactionRuntimeContext) {
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

        async fn delete_owned_resources(&self) -> anyhow::Result<()> {
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

        let (reaction, deprovision_flag) = DeprovisionTestReaction::new(
            drasi_core_types::ComponentName::try_new("deprovision-reaction")
                .expect("fixture component name"),
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();

        delete_reaction(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("deprovision-reaction")
                .expect("fixture component name"),
            true,
        )
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

        let (reaction, deprovision_flag) = DeprovisionTestReaction::new(
            drasi_core_types::ComponentName::try_new("no-deprovision-reaction")
                .expect("fixture component name"),
        );
        add_reaction(&manager, &graph, reaction).await.unwrap();

        delete_reaction(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("no-deprovision-reaction")
                .expect("fixture component name"),
            false,
        )
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
        use crate::test_helpers::updates;
        let (core, trace) = updates::core().await;
        let reaction_name = drasi_core_types::ComponentName::try_new("reconfig-stopped-reaction")
            .expect("fixture component name");
        let old = updates::add(&core, reaction_name.clone(), true).await;
        core.reaction_manager()
            .update_reaction_by_name(
                &reaction_name,
                updates::definition(old.id().clone(), reaction_name.clone(), true, 1),
            )
            .await
            .unwrap();
        updates::settle(&core).await;
        let current = core.component_by_id(old.id()).unwrap();
        assert_eq!(current.id(), old.id());
        assert_ne!(current.generation(), old.generation());
        assert_eq!(
            current.observed().unwrap().lifecycle,
            crate::computation::v1::ComponentLifecycle::Stopped
        );
        assert_eq!(
            trace.events(),
            ["create", "bind", "initialize", "dispose", "create", "bind", "initialize"]
        );
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_reaction_stops_and_restarts_running_reaction() {
        use crate::test_helpers::updates;
        let (core, trace) = updates::core().await;
        let reaction_name = drasi_core_types::ComponentName::try_new("reconfig-running-reaction")
            .expect("fixture component name");
        let old = updates::add(&core, reaction_name.clone(), true).await;
        old.start().await.unwrap();
        old.wait_started().await.unwrap();
        core.reaction_manager()
            .update_reaction_by_name(
                &reaction_name,
                updates::definition(old.id().clone(), reaction_name.clone(), true, 1),
            )
            .await
            .unwrap();
        updates::settle(&core).await;
        let current = core.component_by_id(old.id()).unwrap();
        let started = current.wait_started().await;
        assert!(
            started.is_ok(),
            "{started:?}; state={:?}; hooks={:?}; management={:?}",
            current.observed(),
            trace.events(),
            core.management_status().await
        );
        assert_eq!(
            core.reaction_manager()
                .get_reaction_status_by_name(&reaction_name)
                .await
                .unwrap(),
            ComponentStatus::Running
        );
        assert_eq!(
            trace.events(),
            [
                "create",
                "bind",
                "initialize",
                "start",
                "stop",
                "dispose",
                "create",
                "bind",
                "initialize",
                "start"
            ]
        );
        assert!(old.start().await.is_err());
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_reaction_emits_reconfiguring_event() {
        use crate::test_helpers::updates;
        let (core, _) = updates::core().await;
        let reaction_name = drasi_core_types::ComponentName::try_new("reconfig-event-reaction")
            .expect("fixture component name");
        let reaction = updates::add(&core, reaction_name.clone(), true).await;
        reaction.start().await.unwrap();
        reaction.wait_started().await.unwrap();
        let mut event_rx = core.component_graph().subscribe().unwrap();
        core.reaction_manager()
            .update_reaction_by_name(
                &reaction_name,
                updates::definition(reaction.id().clone(), reaction_name.clone(), true, 1),
            )
            .await
            .unwrap();

        // Wait for the Reconfiguring event via the broadcast channel
        wait_for_component_status_by_id(
            &mut event_rx,
            reaction.id(),
            ComponentStatus::Reconfiguring,
            Duration::from_secs(5),
        )
        .await;
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_reaction_rejects_mismatched_id() {
        use crate::test_helpers::updates;
        let (core, trace) = updates::core().await;
        let reaction_name = drasi_core_types::ComponentName::try_new("original-reaction")
            .expect("fixture component name");
        let _reaction = updates::add(&core, reaction_name.clone(), true).await;
        let before = trace.events();
        let result = core
            .reaction_manager()
            .update_reaction_by_name(
                &reaction_name,
                updates::definition(
                    crate::config::ComponentId::generate(),
                    reaction_name.clone(),
                    true,
                    1,
                ),
            )
            .await;
        assert!(result.is_err(), "Expected error for mismatched IDs");
        assert!(result.unwrap_err().to_string().contains("UUID"));
        assert_eq!(trace.events(), before);
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_nonexistent_reaction() {
        let (manager, _graph) = create_test_manager().await;
        let id = crate::config::ComponentId::generate();
        let reaction_name = drasi_core_types::ComponentName::try_new("nonexistent")
            .expect("fixture component name");
        let definition =
            crate::test_helpers::updates::definition(id.clone(), reaction_name.clone(), true, 1);
        let result = manager
            .update_reaction_by_name(&reaction_name, definition)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // ========================================================================
    // End-to-end bootstrap test — uses the DrasiLib public API with a real
    // query engine and a mock source that feeds bootstrap data.
    // ========================================================================

    /// A reaction that captures snapshot data during bootstrap for verification.
    struct SnapshotTestReaction {
        id: drasi_core_types::ComponentId,
        queries: Vec<drasi_computation_contracts::ComponentReference>,
        status_handle: crate::component_graph::ComponentStatusHandle,
        captured_snapshot: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
        bootstrap_called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl SnapshotTestReaction {
        fn new(
            name: crate::ComponentName,
            queries: Vec<crate::ComponentName>,
        ) -> (
            crate::test_helpers::creation::ReactionFixture<Self>,
            Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            let captured = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let captured_instance = captured.clone();
            let called_instance = called.clone();
            let inputs: Vec<_> = queries
                .into_iter()
                .map(crate::config::ComponentReference::by_name)
                .collect();
            (
                crate::test_helpers::creation::ReactionFixture::new(
                    name,
                    inputs,
                    false,
                    move |id, resolved_inputs| {
                        Ok(Self {
                            id: id.clone(),
                            queries: resolved_inputs
                                .into_iter()
                                .map(crate::config::ComponentReference::by_id)
                                .collect(),
                            status_handle: crate::component_graph::ComponentStatusHandle::new(id),
                            captured_snapshot: captured_instance,
                            bootstrap_called: called_instance,
                        })
                    },
                ),
                captured,
                called,
            )
        }
    }

    #[async_trait]
    impl crate::reactions::Reaction for SnapshotTestReaction {
        fn id(&self) -> &drasi_core_types::ComponentId {
            &self.id
        }
        fn type_name(&self) -> &str {
            "snapshot-test"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
            self.queries.clone()
        }
        fn auto_start(&self) -> bool {
            false
        }
        async fn bind_runtime_context(&self, context: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }
        async fn start(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Running, None)
                .await;
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            self.status_handle
                .set_status(ComponentStatus::Stopped, None)
                .await;
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }
        async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> {
            Ok(())
        }
        fn needs_snapshot_on_fresh_start(&self) -> bool {
            true
        }
        fn default_recovery_policy(&self) -> crate::recovery::ReactionRecoveryPolicy {
            crate::recovery::ReactionRecoveryPolicy::AutoReset
        }

        async fn bootstrap(
            &self,
            ctx: crate::reactions::bootstrap_context::BootstrapContext,
        ) -> Result<()> {
            self.bootstrap_called
                .store(true, std::sync::atomic::Ordering::SeqCst);

            let snapshot = ctx
                .fetch_snapshot()
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let data = snapshot.collect_vec().await;
            let mut guard = self.captured_snapshot.lock().await;
            *guard = data;

            Ok(())
        }
    }

    #[tokio::test]
    async fn test_e2e_bootstrap_snapshot_reaction() {
        use crate::channels::BootstrapEvent;
        use crate::sources::tests::create_test_bootstrap_mock_source;
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };

        // --- Build DrasiLib with a bootstrap mock source, a real query, and the reaction ---

        let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(100);

        let source = create_test_bootstrap_mock_source(
            drasi_core_types::ComponentName::try_new("e2e-src").expect("fixture component name"),
            bootstrap_rx,
        );

        let (reaction, captured_snapshot, bootstrap_called) = SnapshotTestReaction::new(
            drasi_core_types::ComponentName::try_new("snap-reaction")
                .expect("fixture component name"),
            vec![drasi_core_types::ComponentName::try_new("e2e-query")
                .expect("fixture component name")],
        );

        let core = crate::DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("e2e-bootstrap")
            .with_source_fixture(source)
            .with_query(
                crate::Query::cypher(
                    drasi_core_types::ComponentName::try_new("e2e-query")
                        .expect("fixture component name"),
                )
                .query("MATCH (n:Person) RETURN n.name, n.age")
                .from_input_by_name(
                    drasi_core_types::ComponentName::try_new("e2e-src")
                        .expect("fixture component name"),
                )
                .build()
                .unwrap(),
            )
            .with_reaction_fixture(reaction)
            .build()
            .await
            .unwrap();

        // Subscribe to component events before starting.
        let mut event_rx = core.subscribe_all_component_events();
        let source_id = core
            .component_by_name(&crate::ComponentName::try_new("e2e-src").unwrap())
            .unwrap()
            .id()
            .clone();

        // Start — auto-starts source (auto_start=true) and query (auto_start=true by
        // default). The reaction has auto_start=false, so it stays provisioned.
        core.start().await.unwrap();

        // --- Feed bootstrap data through the source channel ---
        let names = vec![("p1", "Alice", 30), ("p2", "Bob", 25), ("p3", "Carol", 35)];
        for (i, (node_id, name, age)) in names.iter().enumerate() {
            let mut props = ElementPropertyMap::default();
            props.insert(
                "name",
                drasi_core::models::ElementValue::String((*name).into()),
            );
            props.insert(
                "age",
                drasi_core::models::ElementValue::Integer((*age) as i64),
            );
            let element = Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("e2e-src", node_id),
                    labels: vec!["Person".into()].into(),
                    effective_from: 1000,
                },
                properties: props,
            };
            bootstrap_tx
                .send(BootstrapEvent {
                    source_id: source_id.clone(),
                    change: SourceChange::Insert { element },
                    timestamp: chrono::Utc::now(),
                    sequence: (i + 1) as u64,
                })
                .await
                .unwrap();
        }
        drop(bootstrap_tx);

        // Wait for the query to reach Running (bootstrap complete).
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &core.component_graph(),
            &crate::ComponentName::try_new("e2e-query").expect("fixture component name"),
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        // Sanity check: the real query should have 3 result rows.
        let results = core
            .get_query_results_by_name(
                &drasi_core_types::ComponentName::try_new("e2e-query")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            3,
            "Query should have 3 result rows from bootstrap"
        );

        // --- Start the reaction — triggers bootstrap → fetch_snapshot ---
        core.start_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("snap-reaction")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

        // --- Verify bootstrap was called and snapshot data matches ---
        assert!(
            bootstrap_called.load(std::sync::atomic::Ordering::SeqCst),
            "bootstrap() should have been called on the reaction"
        );

        let captured = captured_snapshot.lock().await;
        assert_eq!(
            captured.len(),
            3,
            "Reaction bootstrap should have captured 3 snapshot rows"
        );

        let captured_names: Vec<String> = captured
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        for (_, expected_name, _) in &names {
            assert!(
                captured_names.contains(&expected_name.to_string()),
                "Snapshot should contain person '{expected_name}', got: {captured_names:?}"
            );
        }
        drop(captured);
        core.shutdown().await.unwrap();
    }
}
