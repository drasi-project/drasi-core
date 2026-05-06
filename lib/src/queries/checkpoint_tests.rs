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

//! End-to-end tests for the EventSequence / SourcePosition checkpoint separation.
//!
//! These tests validate:
//! - Source position bytes flow through the full pipeline (source → query → result index)
//! - Checkpoints are correctly persisted after query processing
//! - On restart, the source receives the correct `resume_from` position bytes
//! - Sequence numbers are monotonically assigned by the framework
//! - Multiple sources feeding one query keep isolated checkpoints

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;
    use drasi_core::interface::{ResultIndex, ResultSequenceCounter};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::channels::dispatcher::{ChangeDispatcher, ChannelChangeDispatcher};
    use crate::channels::*;
    use crate::config::{QueryConfig, QueryLanguage, SourceSubscriptionConfig};
    use crate::sources::base::{SourceBase, SourceBaseParams};
    use crate::sources::Source;
    use crate::test_helpers::wait_for_component_status;
    use drasi_core::middleware::MiddlewareTypeRegistry;

    // ========================================================================
    // Test source that uses SourceBase and records resume_from
    // ========================================================================

    /// A test source that:
    /// - Uses `SourceBase` for dispatching (so framework assigns sequence)
    /// - Records the `resume_from` bytes received on each subscribe call
    /// - Allows injecting events with `source_position` set
    struct CheckpointTestSource {
        base: SourceBase,
        /// Stores all resume_from values received during subscribe calls
        received_resume_from: Arc<RwLock<Vec<Option<Bytes>>>>,
    }

    impl CheckpointTestSource {
        fn new(id: &str) -> anyhow::Result<Self> {
            let base = SourceBase::new(SourceBaseParams::new(id))?;
            Ok(Self {
                base,
                received_resume_from: Arc::new(RwLock::new(Vec::new())),
            })
        }

        /// Inject a change event with optional source_position bytes.
        async fn inject_change_with_position(
            &self,
            change: drasi_core::models::SourceChange,
            position: Option<Bytes>,
        ) -> anyhow::Result<()> {
            let mut wrapper = SourceEventWrapper::new(
                self.base.get_id().to_string(),
                SourceEvent::Change(change),
                chrono::Utc::now(),
            );
            if let Some(pos) = position {
                wrapper.set_source_position(pos);
            }
            self.base.dispatch_event(wrapper).await
        }

        /// Get all resume_from values received during subscribe calls
        async fn get_resume_from_history(&self) -> Vec<Option<Bytes>> {
            self.received_resume_from.read().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl Source for CheckpointTestSource {
        fn id(&self) -> &str {
            self.base.get_id()
        }

        fn type_name(&self) -> &str {
            "checkpoint-test"
        }

        fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
            std::collections::HashMap::new()
        }

        fn auto_start(&self) -> bool {
            self.base.get_auto_start()
        }

        async fn start(&self) -> anyhow::Result<()> {
            self.base
                .set_status(
                    ComponentStatus::Starting,
                    Some("Starting".to_string()),
                )
                .await;
            self.base
                .set_status(ComponentStatus::Running, Some("Running".to_string()))
                .await;
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            self.base
                .set_status(
                    ComponentStatus::Stopping,
                    Some("Stopping".to_string()),
                )
                .await;
            self.base
                .set_status(ComponentStatus::Stopped, Some("Stopped".to_string()))
                .await;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.base.status_handle().get_status().await
        }

        async fn subscribe(
            &self,
            settings: crate::config::SourceSubscriptionSettings,
        ) -> anyhow::Result<SubscriptionResponse> {
            // Apply settings (sequence recovery, etc.)
            self.base.apply_subscription_settings(&settings);

            // Record the resume_from for test assertions
            self.received_resume_from
                .write()
                .await
                .push(settings.resume_from.clone());

            let receiver = self.base.create_streaming_receiver().await?;
            Ok(SubscriptionResponse {
                query_id: settings.query_id,
                source_id: self.id().to_string(),
                receiver,
                bootstrap_receiver: None,
                position_handle: None,
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn initialize(&self, context: crate::context::SourceRuntimeContext) {
            self.base.initialize(context).await;
        }
    }

    // ========================================================================
    // Test infrastructure helpers
    // ========================================================================

    fn create_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n:Person) RETURN n.name".to_string(),
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
            auto_start: true,
            joins: None,
            enable_bootstrap: false,
            bootstrap_buffer_size: 100,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
        }
    }

    async fn create_test_env() -> (
        Arc<crate::queries::QueryManager>,
        Arc<crate::sources::SourceManager>,
        Arc<tokio::sync::RwLock<crate::component_graph::ComponentGraph>>,
    ) {
        let log_registry = crate::managers::get_or_init_global_registry();
        let (graph, update_rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();
        let graph = Arc::new(tokio::sync::RwLock::new(graph));

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

        let source_manager = Arc::new(crate::sources::SourceManager::new(
            "test-instance",
            log_registry.clone(),
            graph.clone(),
            update_tx.clone(),
        ));

        let index_factory = Arc::new(crate::indexes::IndexFactory::new(vec![], None));
        let middleware_registry = Arc::new(MiddlewareTypeRegistry::new());

        let query_manager = Arc::new(crate::queries::QueryManager::new(
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
        source_manager: &crate::sources::SourceManager,
        graph: &tokio::sync::RwLock<crate::component_graph::ComponentGraph>,
        source: impl Source + 'static,
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
        manager: &crate::queries::QueryManager,
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

    // ========================================================================
    // Tests
    // ========================================================================

    /// Test that the framework assigns monotonically increasing sequence numbers
    /// to events dispatched through SourceBase, independent of source_position.
    #[tokio::test]
    async fn test_sequence_assignment_monotonic() {
        let source = CheckpointTestSource::new("seq-src").unwrap();
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(16);
        source
            .initialize(crate::context::SourceRuntimeContext {
                instance_id: "test".to_string(),
                source_id: "seq-src".to_string(),
                update_tx,
                state_store: None,
                identity_provider: None,
            })
            .await;
        source.start().await.unwrap();

        // Subscribe to get events
        let settings = crate::config::SourceSubscriptionSettings {
            source_id: "seq-src".to_string(),
            enable_bootstrap: false,
            query_id: "q1".to_string(),
            nodes: std::collections::HashSet::new(),
            relations: std::collections::HashSet::new(),
            resume_from: None,
            request_position_handle: false,
            last_sequence: None,
        };
        let sub = source.subscribe(settings).await.unwrap();
        let mut receiver = sub.receiver;

        // Inject 3 events with different positions
        let positions = vec![
            Some(Bytes::from_static(b"pos-A")),
            Some(Bytes::from_static(b"pos-B")),
            None,
        ];

        for (i, pos) in positions.into_iter().enumerate() {
            let change = drasi_core::models::SourceChange::Insert {
                element: drasi_core::models::Element::Node {
                    metadata: drasi_core::models::ElementMetadata {
                        reference: drasi_core::models::ElementReference::new("src", &format!("n{i}")),
                        labels: Arc::new([Arc::from("Person")]),
                        effective_from: 1000 + i as u64,
                    },
                    properties: drasi_core::models::ElementPropertyMap::new(),
                },
            };
            source.inject_change_with_position(change, pos).await.unwrap();
        }

        // Receive events and verify monotonic sequences
        let mut sequences = Vec::new();
        for _ in 0..3 {
            let event = receiver.recv().await.unwrap();
            sequences.push(event.sequence.unwrap());
        }

        // Sequences should be 1, 2, 3 (monotonically increasing, starting at 1)
        assert_eq!(sequences, vec![1, 2, 3]);
    }

    /// Test that source_position bytes attached to events survive the
    /// full dispatch pipeline and are accessible on the receiving end.
    #[tokio::test]
    async fn test_source_position_preserved_through_dispatch() {
        let source = CheckpointTestSource::new("pos-src").unwrap();
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(16);
        source
            .initialize(crate::context::SourceRuntimeContext {
                instance_id: "test".to_string(),
                source_id: "pos-src".to_string(),
                update_tx,
                state_store: None,
                identity_provider: None,
            })
            .await;
        source.start().await.unwrap();

        let settings = crate::config::SourceSubscriptionSettings {
            source_id: "pos-src".to_string(),
            enable_bootstrap: false,
            query_id: "q1".to_string(),
            nodes: std::collections::HashSet::new(),
            relations: std::collections::HashSet::new(),
            resume_from: None,
            request_position_handle: false,
            last_sequence: None,
        };
        let sub = source.subscribe(settings).await.unwrap();
        let mut receiver = sub.receiver;

        // Inject event with a 20-byte MSSQL-like position
        let mssql_pos = Bytes::from(vec![0xDE, 0xAD, 0xBE, 0xEF].repeat(5));
        let change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("src", "node1"),
                    labels: Arc::new([Arc::from("Item")]),
                    effective_from: 2000,
                },
                properties: drasi_core::models::ElementPropertyMap::new(),
            },
        };
        source
            .inject_change_with_position(change, Some(mssql_pos.clone()))
            .await
            .unwrap();

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.source_position, Some(mssql_pos));
        assert!(event.sequence.is_some());
    }

    /// Test the full end-to-end checkpoint persistence:
    /// source emits events with position → query processes → checkpoint persisted → restart → resume_from received.
    #[tokio::test]
    async fn test_checkpoint_persistence_end_to_end() {
        let (query_manager, source_manager, graph) = create_test_env().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.read().await.subscribe();

        // Create and register source
        let source = CheckpointTestSource::new("e2e-source").unwrap();
        add_source(&source_manager, &graph, source).await.unwrap();

        // Start source
        source_manager
            .start_source("e2e-source".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "e2e-source",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Create query subscribed to this source
        let config = create_query_config("e2e-query", vec!["e2e-source".to_string()]);
        add_query(&query_manager, &graph, config).await.unwrap();

        // Start the query
        query_manager
            .start_query("e2e-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "e2e-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Give the processor task time to be ready
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Get source instance to inject events
        let source_instance = source_manager
            .get_source_instance("e2e-source")
            .await
            .unwrap();
        let test_source = source_instance
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();

        // Inject events with position bytes
        let cosmos_position = Bytes::from(b"cosmos-resume-token-abc123xyz".to_vec());
        let change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("e2e-source", "person1"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 1000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![("name".to_string(), drasi_core::evaluation::variable_value::VariableValue::String("Alice".to_string()))]
                        .into_iter()
                        .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_source
            .inject_change_with_position(change, Some(cosmos_position.clone()))
            .await
            .unwrap();

        // Allow time for the query to process the event and persist the checkpoint
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Stop the query
        query_manager
            .stop_query("e2e-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "e2e-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Restart the query — it should read the checkpoint and pass resume_from to source
        query_manager
            .start_query("e2e-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "e2e-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Check what the source received as resume_from on the second subscribe
        let resume_history = test_source.get_resume_from_history().await;
        // First subscribe: None (initial start)
        // Second subscribe: should contain the position bytes from the last processed event
        assert!(
            resume_history.len() >= 2,
            "Expected at least 2 subscribe calls, got {}",
            resume_history.len()
        );
        assert_eq!(
            resume_history[0], None,
            "First subscribe should have no resume_from"
        );
        assert_eq!(
            resume_history[1],
            Some(cosmos_position),
            "Second subscribe should receive the persisted source_position"
        );
    }

    /// Test that checkpoint persistence works with the in-memory result index directly.
    /// This is a focused unit test that doesn't require the full query manager pipeline.
    #[tokio::test]
    async fn test_in_memory_checkpoint_various_sizes() {
        let index = InMemoryResultIndex::new();

        // Postgres-like: 8 bytes (WAL LSN)
        let pg_pos = Bytes::copy_from_slice(&42u64.to_le_bytes());
        index
            .apply_checkpoint(1, "pg-source", Some(&pg_pos))
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 1);
        assert_eq!(cp.get_source_position("pg-source"), Some(&pg_pos));

        // MSSQL-like: 20 bytes
        let mssql_pos = Bytes::from(vec![0xAA; 20]);
        index
            .apply_checkpoint(2, "mssql-source", Some(&mssql_pos))
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 2);
        assert_eq!(cp.get_source_position("mssql-source"), Some(&mssql_pos));
        // Previous source's position should still be stored
        assert_eq!(cp.get_source_position("pg-source"), Some(&pg_pos));

        // MongoDB-like: 80 bytes
        let mongo_pos = Bytes::from(vec![0xBB; 80]);
        index
            .apply_checkpoint(3, "mongo-source", Some(&mongo_pos))
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 3);
        assert_eq!(cp.get_source_position("mongo-source"), Some(&mongo_pos));

        // Cosmos DB-like: 120 bytes (resume token)
        let cosmos_pos = Bytes::from(vec![0xCC; 120]);
        index
            .apply_checkpoint(4, "cosmos-source", Some(&cosmos_pos))
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 4);
        assert_eq!(cp.get_source_position("cosmos-source"), Some(&cosmos_pos));

        // Volatile source: None position removes the entry
        index
            .apply_checkpoint(5, "volatile-source", None)
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 5);
        assert_eq!(cp.get_source_position("volatile-source"), None);
        // Others still present
        assert_eq!(cp.get_source_position("cosmos-source"), Some(&cosmos_pos));
    }

    /// Test that get_sequence and get_checkpoint are consistent views of the same data.
    #[tokio::test]
    async fn test_checkpoint_and_sequence_consistency() {
        let index = InMemoryResultIndex::new();

        // Writing via apply_checkpoint should be readable via get_sequence
        let pos = Bytes::from_static(b"test-position");
        index
            .apply_checkpoint(42, "src-a", Some(&pos))
            .await
            .unwrap();

        let seq = index.get_sequence().await.unwrap();
        assert_eq!(seq.sequence, 42);
        assert_eq!(seq.source_change_id.as_ref(), "src-a");

        // Writing via apply_sequence should be readable via get_checkpoint
        index.apply_sequence(99, "src-b").await.unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 99);
        assert_eq!(cp.source_change_id.as_ref(), "src-b");
    }

    /// Test that resume_from is correctly passed to source on initial subscribe
    /// when no checkpoint exists (should be None).
    #[tokio::test]
    async fn test_initial_subscribe_no_checkpoint() {
        let (query_manager, source_manager, graph) = create_test_env().await;

        let mut event_rx = graph.read().await.subscribe();

        let source = CheckpointTestSource::new("fresh-source").unwrap();
        add_source(&source_manager, &graph, source).await.unwrap();
        source_manager
            .start_source("fresh-source".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "fresh-source",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let config = create_query_config("fresh-query", vec!["fresh-source".to_string()]);
        add_query(&query_manager, &graph, config).await.unwrap();
        query_manager
            .start_query("fresh-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "fresh-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Source should have received a subscribe call with resume_from: None
        let source_instance = source_manager
            .get_source_instance("fresh-source")
            .await
            .unwrap();
        let test_source = source_instance
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();
        let history = test_source.get_resume_from_history().await;
        assert!(!history.is_empty());
        assert_eq!(history[0], None, "Initial subscribe should have no resume_from");
    }
}
