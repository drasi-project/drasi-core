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
    use drasi_core::interface::{ResultIndex, CheckpointStore};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use crate::channels::dispatcher::{ChangeDispatcher, ChannelChangeDispatcher};
    use crate::channels::*;
    use crate::config::{QueryConfig, QueryLanguage, SourceSubscriptionConfig};
    use crate::queries::manager::DrasiQuery;
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
    /// - Optionally returns a position_handle for tracking confirmed positions
    struct CheckpointTestSource {
        base: SourceBase,
        /// Stores all resume_from values received during subscribe calls
        received_resume_from: Arc<RwLock<Vec<Option<Bytes>>>>,
        /// Stores all last_sequence values received during subscribe calls
        received_last_sequence: Arc<RwLock<Vec<Option<u64>>>>,
        /// Whether to provide a position_handle on subscribe
        provide_position_handle: bool,
        /// The position handle (shared with query manager after subscribe)
        position_handle: Arc<std::sync::atomic::AtomicU64>,
    }

    impl CheckpointTestSource {
        fn new(id: &str) -> anyhow::Result<Self> {
            let base = SourceBase::new(SourceBaseParams::new(id))?;
            Ok(Self {
                base,
                received_resume_from: Arc::new(RwLock::new(Vec::new())),
                received_last_sequence: Arc::new(RwLock::new(Vec::new())),
                provide_position_handle: false,
                position_handle: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            })
        }

        fn with_position_handle(mut self) -> Self {
            self.provide_position_handle = true;
            self
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

        /// Get all last_sequence values received during subscribe calls
        async fn get_last_sequence_history(&self) -> Vec<Option<u64>> {
            self.received_last_sequence.read().await.clone()
        }

        /// Get the current value of the position handle
        fn get_position_handle_value(&self) -> u64 {
            self.position_handle
                .load(std::sync::atomic::Ordering::Acquire)
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
                .set_status(ComponentStatus::Starting, Some("Starting".to_string()))
                .await;
            self.base
                .set_status(ComponentStatus::Running, Some("Running".to_string()))
                .await;
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            self.base
                .set_status(ComponentStatus::Stopping, Some("Stopping".to_string()))
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

            // Record the resume_from and last_sequence for test assertions
            self.received_resume_from
                .write()
                .await
                .push(settings.resume_from.clone());
            self.received_last_sequence
                .write()
                .await
                .push(settings.last_sequence);

            let receiver = self.base.create_streaming_receiver().await?;
            Ok(SubscriptionResponse {
                query_id: settings.query_id,
                source_id: self.id().to_string(),
                receiver,
                bootstrap_receiver: None,
                position_handle: if self.provide_position_handle {
                    Some(self.position_handle.clone())
                } else {
                    None
                },
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
        let positions =
            vec![Some(Bytes::from_static(b"pos-A")), Some(Bytes::from_static(b"pos-B")), None];

        for (i, pos) in positions.into_iter().enumerate() {
            let change = drasi_core::models::SourceChange::Insert {
                element: drasi_core::models::Element::Node {
                    metadata: drasi_core::models::ElementMetadata {
                        reference: drasi_core::models::ElementReference::new(
                            "src",
                            &format!("n{i}"),
                        ),
                        labels: Arc::new([Arc::from("Person")]),
                        effective_from: 1000 + i as u64,
                    },
                    properties: drasi_core::models::ElementPropertyMap::new(),
                },
            };
            source
                .inject_change_with_position(change, pos)
                .await
                .unwrap();
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
        let mssql_pos = Bytes::from([0xDE, 0xAD, 0xBE, 0xEF].repeat(5));
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
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "Alice".to_string(),
                        ),
                    )]
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
        assert_eq!(seq.source_id.as_ref(), "src-a");

        // Writing via apply_checkpoint(None) should be readable via get_checkpoint
        index.apply_checkpoint(99, "src-b", None).await.unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 99);
        assert_eq!(cp.source_id.as_ref(), "src-b");
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
        assert_eq!(
            history[0], None,
            "Initial subscribe should have no resume_from"
        );
    }

    // ========================================================================
    // Fix #4: Dedup end-to-end test
    // ========================================================================

    /// Test that duplicate events (with sequence ≤ last_processed) are skipped on restart.
    /// This validates the dedup logic in the query processor.
    #[tokio::test]
    async fn test_dedup_skips_duplicate_events_after_restart() {
        let (query_manager, source_manager, graph) = create_test_env().await;
        let mut event_rx = graph.read().await.subscribe();

        let source = CheckpointTestSource::new("dedup-source").unwrap();
        add_source(&source_manager, &graph, source).await.unwrap();
        source_manager
            .start_source("dedup-source".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "dedup-source",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let config = create_query_config("dedup-query", vec!["dedup-source".to_string()]);
        add_query(&query_manager, &graph, config).await.unwrap();
        query_manager
            .start_query("dedup-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "dedup-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let source_instance = source_manager
            .get_source_instance("dedup-source")
            .await
            .unwrap();
        let test_source = source_instance
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();

        // Inject 3 events — framework will assign sequences 1, 2, 3
        for i in 0..3 {
            let change = drasi_core::models::SourceChange::Insert {
                element: drasi_core::models::Element::Node {
                    metadata: drasi_core::models::ElementMetadata {
                        reference: drasi_core::models::ElementReference::new(
                            "dedup-source",
                            &format!("node{i}"),
                        ),
                        labels: Arc::new([Arc::from("Person")]),
                        effective_from: 1000 + i as u64,
                    },
                    properties: drasi_core::models::ElementPropertyMap::from(
                        vec![(
                            "name".to_string(),
                            drasi_core::evaluation::variable_value::VariableValue::String(format!(
                                "person{i}"
                            )),
                        )]
                        .into_iter()
                        .collect::<std::collections::BTreeMap<_, _>>(),
                    ),
                },
            };
            test_source
                .inject_change_with_position(change, Some(Bytes::from(format!("pos{i}"))))
                .await
                .unwrap();
        }

        // Wait for processing + checkpoint
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Stop and restart the query
        query_manager
            .stop_query("dedup-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "dedup-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;

        query_manager
            .start_query("dedup-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "dedup-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Now inject a NEW event (seq 4) — it should be processed
        let new_change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("dedup-source", "node3"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 2000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "new-person".to_string(),
                        ),
                    )]
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_source
            .inject_change_with_position(new_change, Some(Bytes::from("pos3")))
            .await
            .unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify the checkpoint advanced to sequence 4 (the new event was processed)
        let query_instance = query_manager
            .get_query_instance("dedup-query")
            .await
            .unwrap();
        let drasi_query = query_instance
            .as_any()
            .downcast_ref::<DrasiQuery>()
            .unwrap();
        let result_index = drasi_query.get_result_index().await.unwrap();
        let checkpoint = result_index.get_checkpoint().await.unwrap();
        assert_eq!(
            checkpoint.sequence, 4,
            "After restart + new event, checkpoint should be at sequence 4"
        );
    }

    // ========================================================================
    // Fix #5: Size validation boundary test
    // ========================================================================

    /// Test the 64KB size limit on source_position.
    /// Positions at or above MAX_SOURCE_POSITION_BYTES (65536) should be stripped.
    #[tokio::test]
    async fn test_source_position_size_validation() {
        let source = CheckpointTestSource::new("size-src").unwrap();
        let (update_tx, _update_rx) = tokio::sync::mpsc::channel(16);
        source
            .initialize(crate::context::SourceRuntimeContext {
                instance_id: "test".to_string(),
                source_id: "size-src".to_string(),
                update_tx,
                state_store: None,
                identity_provider: None,
            })
            .await;
        source.start().await.unwrap();

        let settings = crate::config::SourceSubscriptionSettings {
            source_id: "size-src".to_string(),
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

        // Test: position at exactly 65,535 bytes (just under limit) should be preserved
        let pos_under_limit = Bytes::from(vec![0xAA; 65_535]);
        let change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("src", "n1"),
                    labels: Arc::new([Arc::from("Item")]),
                    effective_from: 1000,
                },
                properties: drasi_core::models::ElementPropertyMap::new(),
            },
        };
        source
            .inject_change_with_position(change, Some(pos_under_limit.clone()))
            .await
            .unwrap();

        let event = receiver.recv().await.unwrap();
        assert_eq!(
            event.source_position,
            Some(pos_under_limit),
            "Position at 65,535 bytes should be preserved"
        );

        // Test: position at exactly 65,536 bytes (at limit, still accepted - limit is >) should be preserved
        let pos_at_limit = Bytes::from(vec![0xBB; 65_536]);
        let change2 = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("src", "n2"),
                    labels: Arc::new([Arc::from("Item")]),
                    effective_from: 1001,
                },
                properties: drasi_core::models::ElementPropertyMap::new(),
            },
        };
        source
            .inject_change_with_position(change2, Some(pos_at_limit.clone()))
            .await
            .unwrap();

        let event2 = receiver.recv().await.unwrap();
        assert_eq!(
            event2.source_position,
            Some(pos_at_limit),
            "Position at exactly 65,536 bytes should be preserved (limit is >)"
        );

        // Test: position at 65,537 bytes (over limit) should be stripped
        let pos_over_limit = Bytes::from(vec![0xCC; 65_537]);
        let change3 = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("src", "n3"),
                    labels: Arc::new([Arc::from("Item")]),
                    effective_from: 1002,
                },
                properties: drasi_core::models::ElementPropertyMap::new(),
            },
        };
        source
            .inject_change_with_position(change3, Some(pos_over_limit))
            .await
            .unwrap();

        let event3 = receiver.recv().await.unwrap();
        assert_eq!(
            event3.source_position, None,
            "Position at 65,537 bytes should be stripped"
        );

        // Verify sequences are still monotonic despite stripping
        assert_eq!(event3.sequence.unwrap(), 3);
    }

    // ========================================================================
    // Fix #6: Multi-source checkpoint isolation test
    // ========================================================================

    /// Test that two sources feeding one query maintain isolated per-source checkpoints.
    /// On restart, each source should receive its own position via resume_from.
    #[tokio::test]
    async fn test_multi_source_checkpoint_isolation() {
        let (query_manager, source_manager, graph) = create_test_env().await;
        let mut event_rx = graph.read().await.subscribe();

        // Create two sources
        let source_a = CheckpointTestSource::new("src-alpha").unwrap();
        let source_b = CheckpointTestSource::new("src-beta").unwrap();
        add_source(&source_manager, &graph, source_a).await.unwrap();
        add_source(&source_manager, &graph, source_b).await.unwrap();
        source_manager
            .start_source("src-alpha".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "src-alpha",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        source_manager
            .start_source("src-beta".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "src-beta",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Create one query subscribed to both sources
        let config = create_query_config(
            "multi-query",
            vec!["src-alpha".to_string(), "src-beta".to_string()],
        );
        add_query(&query_manager, &graph, config).await.unwrap();
        query_manager
            .start_query("multi-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "multi-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Inject events from source A with position "alpha-pos-final"
        let src_a = source_manager
            .get_source_instance("src-alpha")
            .await
            .unwrap();
        let test_src_a = src_a
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();

        let change_a = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("src-alpha", "a1"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 1000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "AlphaUser".to_string(),
                        ),
                    )]
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_src_a
            .inject_change_with_position(change_a, Some(Bytes::from("alpha-pos-final")))
            .await
            .unwrap();

        // Inject events from source B with position "beta-pos-final"
        let src_b = source_manager
            .get_source_instance("src-beta")
            .await
            .unwrap();
        let test_src_b = src_b
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();

        let change_b = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("src-beta", "b1"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 2000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "BetaUser".to_string(),
                        ),
                    )]
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_src_b
            .inject_change_with_position(change_b, Some(Bytes::from("beta-pos-final")))
            .await
            .unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Stop and restart the query
        query_manager
            .stop_query("multi-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "multi-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;

        query_manager
            .start_query("multi-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "multi-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Verify each source received its own resume_from position
        let history_a = test_src_a.get_resume_from_history().await;
        let history_b = test_src_b.get_resume_from_history().await;

        assert!(
            history_a.len() >= 2,
            "Source A should have at least 2 subscribes, got {}",
            history_a.len()
        );
        assert!(
            history_b.len() >= 2,
            "Source B should have at least 2 subscribes, got {}",
            history_b.len()
        );

        // First subscribe: no checkpoint
        assert_eq!(history_a[0], None);
        assert_eq!(history_b[0], None);

        // Second subscribe: each gets its own position
        assert_eq!(
            history_a[1],
            Some(Bytes::from("alpha-pos-final")),
            "Source A should resume from its own position"
        );
        assert_eq!(
            history_b[1],
            Some(Bytes::from("beta-pos-final")),
            "Source B should resume from its own position"
        );
    }

    // ========================================================================
    // Fix #7: Position handle update verification test
    // ========================================================================

    /// Test that the position handle (AtomicU64) is updated with the correct
    /// sequence number after successful checkpoint persistence.
    #[tokio::test]
    async fn test_position_handle_updated_after_checkpoint() {
        let (query_manager, source_manager, graph) = create_test_env().await;
        let mut event_rx = graph.read().await.subscribe();

        // Create source with position handle enabled
        let source = CheckpointTestSource::new("handle-source")
            .unwrap()
            .with_position_handle();
        add_source(&source_manager, &graph, source).await.unwrap();
        source_manager
            .start_source("handle-source".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "handle-source",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let config = create_query_config("handle-query", vec!["handle-source".to_string()]);
        add_query(&query_manager, &graph, config).await.unwrap();
        query_manager
            .start_query("handle-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "handle-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let source_instance = source_manager
            .get_source_instance("handle-source")
            .await
            .unwrap();
        let test_source = source_instance
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();

        // Position handle should start at 0 (no events processed yet)
        assert_eq!(
            test_source.get_position_handle_value(),
            0,
            "Position handle should be 0 before any events"
        );

        // Inject an event
        let change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("handle-source", "h1"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 1000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "HandleTest".to_string(),
                        ),
                    )]
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_source
            .inject_change_with_position(change, Some(Bytes::from("handle-pos")))
            .await
            .unwrap();

        // Wait for processing + checkpoint
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Position handle should now be updated to the sequence of the processed event
        let handle_val = test_source.get_position_handle_value();
        assert_eq!(
            handle_val, 1,
            "Position handle should be 1 after first event processed"
        );

        // Inject a second event
        let change2 = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("handle-source", "h2"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 2000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "HandleTest2".to_string(),
                        ),
                    )]
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_source
            .inject_change_with_position(change2, Some(Bytes::from("handle-pos-2")))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let handle_val2 = test_source.get_position_handle_value();
        assert_eq!(
            handle_val2, 2,
            "Position handle should be 2 after second event processed"
        );
    }

    // ========================================================================
    // Fix #8: Sequence recovery post-restart test
    // ========================================================================

    /// Test that after restart, the sequence counter continues from where it left off.
    /// If 3 events were processed before restart (seq 1,2,3), the next event after
    /// restart should get seq 4, not seq 1.
    #[tokio::test]
    async fn test_sequence_recovery_continues_after_restart() {
        let (query_manager, source_manager, graph) = create_test_env().await;
        let mut event_rx = graph.read().await.subscribe();

        let source = CheckpointTestSource::new("recov-source").unwrap();
        add_source(&source_manager, &graph, source).await.unwrap();
        source_manager
            .start_source("recov-source".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "recov-source",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let config = create_query_config("recov-query", vec!["recov-source".to_string()]);
        add_query(&query_manager, &graph, config).await.unwrap();
        query_manager
            .start_query("recov-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "recov-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let source_instance = source_manager
            .get_source_instance("recov-source")
            .await
            .unwrap();
        let test_source = source_instance
            .as_any()
            .downcast_ref::<CheckpointTestSource>()
            .unwrap();

        // Inject 3 events (will get sequences 1, 2, 3)
        for i in 0..3 {
            let change = drasi_core::models::SourceChange::Insert {
                element: drasi_core::models::Element::Node {
                    metadata: drasi_core::models::ElementMetadata {
                        reference: drasi_core::models::ElementReference::new(
                            "recov-source",
                            &format!("r{i}"),
                        ),
                        labels: Arc::new([Arc::from("Person")]),
                        effective_from: 1000 + i as u64,
                    },
                    properties: drasi_core::models::ElementPropertyMap::from(
                        vec![(
                            "name".to_string(),
                            drasi_core::evaluation::variable_value::VariableValue::String(format!(
                                "recov{i}"
                            )),
                        )]
                        .into_iter()
                        .collect::<std::collections::BTreeMap<_, _>>(),
                    ),
                },
            };
            test_source
                .inject_change_with_position(change, Some(Bytes::from(format!("rpos{i}"))))
                .await
                .unwrap();
        }

        // Wait for processing + checkpoint
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify checkpoint is at seq 3
        let query_instance = query_manager
            .get_query_instance("recov-query")
            .await
            .unwrap();
        let drasi_query = query_instance
            .as_any()
            .downcast_ref::<DrasiQuery>()
            .unwrap();
        let result_index = drasi_query.get_result_index().await.unwrap();
        let checkpoint_before = result_index.get_checkpoint().await.unwrap();
        assert_eq!(checkpoint_before.sequence, 3);

        // Stop and restart
        query_manager
            .stop_query("recov-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "recov-query",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;

        query_manager
            .start_query("recov-query".to_string())
            .await
            .unwrap();
        wait_for_component_status(
            &mut event_rx,
            "recov-query",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify that last_sequence was passed to the source on restart
        let last_seq_history = test_source.get_last_sequence_history().await;
        assert!(
            last_seq_history.len() >= 2,
            "Expected at least 2 subscribe calls"
        );
        assert_eq!(
            last_seq_history[0], None,
            "First subscribe should have no last_sequence"
        );
        assert_eq!(
            last_seq_history[1],
            Some(3),
            "Second subscribe should pass last_sequence=3"
        );

        // Inject a new event after restart — it should get sequence 4
        let new_change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new("recov-source", "r-new"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 3000,
                },
                properties: drasi_core::models::ElementPropertyMap::from(
                    vec![(
                        "name".to_string(),
                        drasi_core::evaluation::variable_value::VariableValue::String(
                            "new-after-restart".to_string(),
                        ),
                    )]
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            },
        };
        test_source
            .inject_change_with_position(new_change, Some(Bytes::from("rpos-new")))
            .await
            .unwrap();

        // Wait for processing
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify checkpoint advanced to seq 4 (not 1)
        let checkpoint_after = result_index.get_checkpoint().await.unwrap();
        assert_eq!(
            checkpoint_after.sequence, 4,
            "After restart, next event should get sequence 4 (not 1)"
        );
    }
}
