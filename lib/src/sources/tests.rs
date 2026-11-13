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

//! Test utilities for source testing.
//!
//! This module provides `TestMockSource` which is shared across multiple test modules:
//! - sources/tests.rs (this file)
//! - queries/tests.rs
//! - queries/joins_test.rs

use crate::channels::dispatcher::{ChangeDispatcher, ChannelChangeDispatcher};
use crate::channels::*;
use crate::sources::Source;
use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// A simple test mock source for unit testing.
///
/// This mock source supports event injection for testing data flow through queries.
/// Status events are emitted through the component graph (set via initialize()).
pub struct TestMockSource {
    id: drasi_core_types::ComponentId,
    auto_start: bool,
    /// Status handle — always available, wired to graph during initialize().
    status_handle: crate::component_graph::ComponentStatusHandle,
    /// Dispatchers for sending events to subscribed queries
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    next_sequence: Mutex<u64>,
}

impl TestMockSource {
    pub(crate) fn fixture(
        name: impl AsRef<str>,
        auto_start: bool,
    ) -> crate::test_helpers::creation::SourceFixture<Self> {
        crate::test_helpers::creation::SourceFixture::new(
            drasi_core_types::ComponentName::try_new(name.as_ref())
                .expect("fixture component name"),
            auto_start,
            move |id| Self::with_auto_start(id, auto_start),
        )
    }

    pub fn new(id: drasi_core_types::ComponentId) -> Result<Self> {
        let status_handle = crate::component_graph::ComponentStatusHandle::new(id.clone());
        Ok(Self {
            id,
            auto_start: true,
            status_handle,
            dispatchers: Arc::new(RwLock::new(Vec::new())),
            next_sequence: Mutex::new(1),
        })
    }

    /// Create a new test mock source with configurable auto_start
    pub fn with_auto_start(id: drasi_core_types::ComponentId, auto_start: bool) -> Result<Self> {
        let status_handle = crate::component_graph::ComponentStatusHandle::new(id.clone());
        Ok(Self {
            id,
            auto_start,
            status_handle,
            dispatchers: Arc::new(RwLock::new(Vec::new())),
            next_sequence: Mutex::new(1),
        })
    }

    /// Inject an event into all subscribed queries.
    pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
        // Keep assignment and fanout serialized so every subscriber sees the
        // same source-owned sequence, including concurrent injections.
        let mut next_sequence = self.next_sequence.lock().await;
        let sequence = *next_sequence;
        *next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("mock source sequence exhausted"))?;
        let dispatchers = self.dispatchers.read().await;
        let wrapper = SourceEventWrapper::new(
            self.id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
            sequence,
        );
        let arc_wrapper = Arc::new(wrapper);
        for dispatcher in dispatchers.iter() {
            dispatcher.dispatch_change(arc_wrapper.clone()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Source for TestMockSource {
    fn id(&self) -> &drasi_core_types::ComponentId {
        &self.id
    }

    fn type_name(&self) -> &str {
        "mock"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.auto_start
    }

    async fn start(&self) -> Result<()> {
        self.status_handle
            .set_status(
                ComponentStatus::Starting,
                Some("Starting source".to_string()),
            )
            .await;
        self.status_handle
            .set_status(ComponentStatus::Running, Some("Source started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.status_handle
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping source".to_string()),
            )
            .await;
        self.status_handle
            .set_status(ComponentStatus::Stopped, Some("Source stopped".to_string()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status_handle.get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let mut next_sequence = self.next_sequence.lock().await;
        if let Some(resume_sequence) = settings.resume_sequence {
            let resume_next = resume_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("mock source sequence exhausted"))?;
            *next_sequence = (*next_sequence).max(resume_next);
        }
        let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(100);
        let receiver = dispatcher.create_receiver().await?;

        self.dispatchers.write().await.push(Box::new(dispatcher));

        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id.clone(),
            receiver,
            bootstrap_receiver: None,
            position_handle: None,
            bootstrap_result_receiver: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn bind_runtime_context(&self, context: crate::context::SourceRuntimeContext) {
        self.status_handle.wire(context.update_tx.clone()).await;
    }
}

/// Helper to create a TestMockSource instance
pub(crate) fn create_test_mock_source(
    name: String,
) -> crate::test_helpers::creation::SourceFixture<TestMockSource> {
    TestMockSource::fixture(name, true)
}

/// A test mock source that provides a bootstrap channel for testing the bootstrap gate.
///
/// Unlike `TestMockSource` (which returns `bootstrap_receiver: None`), this source
/// accepts a pre-created `BootstrapEventReceiver` and returns it from `subscribe()`,
/// allowing tests to control bootstrap timing.
pub struct TestBootstrapMockSource {
    id: drasi_core_types::ComponentId,
    auto_start: bool,
    /// Status handle — always available, wired to graph during initialize().
    status_handle: crate::component_graph::ComponentStatusHandle,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    bootstrap_rx: Arc<tokio::sync::Mutex<Option<BootstrapEventReceiver>>>,
}

impl TestBootstrapMockSource {
    pub fn new(
        id: drasi_core_types::ComponentId,
        bootstrap_rx: BootstrapEventReceiver,
    ) -> Result<Self> {
        let status_handle = crate::component_graph::ComponentStatusHandle::new(id.clone());
        Ok(Self {
            id,
            auto_start: true,
            status_handle,
            dispatchers: Arc::new(RwLock::new(Vec::new())),
            bootstrap_rx: Arc::new(tokio::sync::Mutex::new(Some(bootstrap_rx))),
        })
    }
}

#[async_trait]
impl Source for TestBootstrapMockSource {
    fn id(&self) -> &drasi_core_types::ComponentId {
        &self.id
    }

    fn type_name(&self) -> &str {
        "mock-bootstrap"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.auto_start
    }

    async fn start(&self) -> Result<()> {
        self.status_handle
            .set_status(
                ComponentStatus::Starting,
                Some("Starting source".to_string()),
            )
            .await;
        self.status_handle
            .set_status(ComponentStatus::Running, Some("Source started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.status_handle
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping source".to_string()),
            )
            .await;
        self.status_handle
            .set_status(ComponentStatus::Stopped, Some("Source stopped".to_string()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status_handle.get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(100);
        let receiver = dispatcher.create_receiver().await?;

        self.dispatchers.write().await.push(Box::new(dispatcher));

        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id.clone(),
            receiver,
            bootstrap_receiver: self.bootstrap_rx.lock().await.take(),
            position_handle: None,
            bootstrap_result_receiver: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn bind_runtime_context(&self, context: crate::context::SourceRuntimeContext) {
        self.status_handle.wire(context.update_tx.clone()).await;
    }
}

/// Helper to create a TestBootstrapMockSource instance
pub fn create_test_bootstrap_mock_source(
    name: drasi_core_types::ComponentName,
    bootstrap_rx: BootstrapEventReceiver,
) -> crate::test_helpers::creation::SourceFixture<TestBootstrapMockSource> {
    crate::test_helpers::creation::SourceFixture::new(name, true, move |id| {
        TestBootstrapMockSource::new(id.clone(), bootstrap_rx)
    })
}

/// A test source that uses SourceBase for logging integration tests.
///
/// This source uses the full SourceBase infrastructure including logger support.
pub struct LoggingTestSource {
    base: crate::sources::SourceBase,
    fail_start: bool,
}

impl LoggingTestSource {
    fn fixture(name: impl AsRef<str>) -> crate::test_helpers::creation::SourceFixture<Self> {
        crate::test_helpers::creation::SourceFixture::new(
            drasi_core_types::ComponentName::try_new(name.as_ref())
                .expect("fixture component name"),
            true,
            Self::new,
        )
    }

    pub fn new(id: drasi_core_types::ComponentId) -> Result<Self> {
        let params = crate::sources::SourceBaseParams::new(id);
        let base = crate::sources::SourceBase::new(params)?;
        Ok(Self {
            base,
            fail_start: false,
        })
    }
}

#[async_trait]
impl Source for LoggingTestSource {
    fn id(&self) -> &drasi_core_types::ComponentId {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "logging-test"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.auto_start
    }

    async fn start(&self) -> Result<()> {
        anyhow::ensure!(
            !self.fail_start,
            "injected failed start without local status change"
        );
        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "logging-test")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn bind_runtime_context(&self, context: crate::context::SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[tokio::test]
    async fn injected_sequence_is_source_owned_across_fanout_restart_and_resume() {
        use crate::config::SourceSubscriptionSettings;
        use drasi_core::models::{ElementMetadata, ElementReference};

        let id = crate::config::ComponentId::generate();
        let source = TestMockSource::new(id.clone()).unwrap();
        let settings =
            |query_id: drasi_core_types::ComponentId, resume_sequence| SourceSubscriptionSettings {
                source_id: id.clone(),
                query_id,
                enable_bootstrap: false,
                nodes: Default::default(),
                relations: Default::default(),
                resume_from: None,
                resume_sequence,
                request_position_handle: false,
            };
        let mut first = source
            .subscribe(settings(crate::config::ComponentId::generate(), None))
            .await
            .unwrap()
            .receiver;
        let mut second = source
            .subscribe(settings(crate::config::ComponentId::generate(), None))
            .await
            .unwrap()
            .receiver;
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("sequenced", "node"),
                labels: Default::default(),
                effective_from: 1_000,
            },
        };
        source.start().await.unwrap();
        for expected in 1..=3 {
            if expected == 3 {
                source.stop().await.unwrap();
                source.start().await.unwrap();
            }
            source.inject_event(change.clone()).await.unwrap();
            let one = first.recv().await.unwrap();
            let two = second.recv().await.unwrap();
            assert_eq!(one.sequence, expected);
            assert_eq!(one.source_id, id);
            assert_eq!(one.event, SourceEvent::Change(change.clone()));
            assert!(Arc::ptr_eq(&one, &two));
        }

        let mut resumed = source
            .subscribe(settings(crate::config::ComponentId::generate(), Some(500)))
            .await
            .unwrap()
            .receiver;
        source.inject_event(change).await.unwrap();
        for receiver in [&mut first, &mut second, &mut resumed] {
            assert_eq!(receiver.recv().await.unwrap().sequence, 501);
        }
        source.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_mock_source_sequences_start_at_one() {
        use crate::config::SourceSubscriptionSettings;
        use drasi_core::models::{ElementMetadata, ElementReference};

        let sources = [
            TestMockSource::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000027b")
                    .expect("fixture UUID"),
            )
            .unwrap(),
            TestMockSource::with_auto_start(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000373")
                    .expect("fixture UUID"),
                true,
            )
            .unwrap(),
            TestMockSource::with_auto_start(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000027c")
                    .expect("fixture UUID"),
                false,
            )
            .unwrap(),
        ];

        for source in sources {
            let mut receiver = source
                .subscribe(SourceSubscriptionSettings {
                    source_id: source.id().clone(),
                    query_id: drasi_core_types::ComponentId::try_new(
                        "00000000-0000-0000-0000-000000000277",
                    )
                    .expect("fixture UUID"),
                    enable_bootstrap: false,
                    nodes: Default::default(),
                    relations: Default::default(),
                    resume_sequence: None,
                    request_position_handle: false,
                    resume_from: None,
                })
                .await
                .unwrap()
                .receiver;

            for expected_sequence in 1..=2 {
                source
                    .inject_event(SourceChange::Delete {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("mock-source", "test-node"),
                            labels: vec![].into(),
                            effective_from: 0,
                        },
                    })
                    .await
                    .unwrap();

                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
                        .await
                        .expect("timed out waiting for injected event")
                        .expect("event stream closed unexpectedly");
                assert_eq!(event.sequence, expected_sequence, "source {}", source.id());
            }
        }
    }

    #[test]
    fn test_source_supports_replay_default_true() {
        let source = TestMockSource::new(crate::config::ComponentId::generate().clone()).unwrap();
        assert!(source.supports_replay());
    }

    #[test]
    fn test_source_error_position_unavailable_display() {
        use crate::sources::SourceError;
        use bytes::Bytes;

        let err = SourceError::PositionUnavailable {
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-000000000278",
            )
            .expect("fixture UUID"),
            requested: Bytes::copy_from_slice(&1000u64.to_le_bytes()),
            earliest_available: Some(Bytes::copy_from_slice(&5000u64.to_le_bytes())),
        };

        // Verify display formatting
        let msg = format!("{err}");
        assert!(msg.contains("00000000-0000-0000-0000-000000000278"));
        assert!(msg.contains("position unavailable"));

        // Verify anyhow round-trip: SourceError → anyhow::Error → downcast
        let anyhow_err: anyhow::Error = err.into();
        let downcasted = anyhow_err.downcast_ref::<SourceError>();
        assert!(downcasted.is_some());
        match downcasted.unwrap() {
            SourceError::PositionUnavailable {
                source_id,
                requested,
                earliest_available,
            } => {
                assert_eq!(
                    source_id,
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000278")
                        .expect("fixture UUID")
                );
                assert_eq!(*requested, Bytes::copy_from_slice(&1000u64.to_le_bytes()));
                assert!(earliest_available.is_some());
            }
        }
    }

    #[test]
    fn test_source_error_position_unavailable_no_earliest() {
        use crate::sources::SourceError;
        use bytes::Bytes;

        let err = SourceError::PositionUnavailable {
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-000000000279",
            )
            .expect("fixture UUID"),
            requested: Bytes::from_static(b"some-position"),
            earliest_available: None,
        };

        let msg = format!("{err}");
        assert!(msg.contains("None"));
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::test_helpers::managers::SourceManager;
    use crate::test_helpers::wait_for_component_status_by_id;

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counter for generating unique test IDs
    static TEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Generate a unique component name for tests to avoid conflicts in parallel test runs.
    fn unique_name(prefix: &str) -> drasi_core_types::ComponentName {
        let counter = TEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        drasi_core_types::ComponentName::try_new(format!("{prefix}-{counter}"))
            .expect("generated fixture component name")
    }

    async fn create_test_manager() -> (
        Arc<SourceManager>,
        crate::component_graph::ComponentGraphView,
    ) {
        let core = crate::test_helpers::managers::empty_core().await;
        (
            Arc::new(SourceManager(core.clone())),
            core.component_graph(),
        )
    }

    /// Helper: register a source in the graph, then provision it in the manager.
    async fn add_source(
        manager: &SourceManager,
        _graph: &crate::component_graph::ComponentGraphView,
        source: crate::test_helpers::creation::SourceFixture<impl Source + 'static>,
    ) -> anyhow::Result<()> {
        manager.add(source).await
    }

    /// Helper: teardown a source in the manager, then deregister from the graph.
    async fn delete_source(
        manager: &SourceManager,
        _graph: &crate::component_graph::ComponentGraphView,
        id: &drasi_core_types::ComponentName,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        manager.teardown_source_by_name(id, cleanup).await
    }

    #[tokio::test]
    async fn test_add_source() {
        let (manager, graph) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string());
        let result = add_source(&manager, &graph, source).await;

        assert!(result.is_ok());

        // Verify source was added
        let sources = manager.list_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].name,
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name")
        );
    }

    #[tokio::test]
    async fn test_add_duplicate_source() {
        let (manager, graph) = create_test_manager().await;

        let source1 = create_test_mock_source("test-source".to_string());
        let source2 = create_test_mock_source("test-source".to_string());

        // Add source first time
        assert!(add_source(&manager, &graph, source1).await.is_ok());

        // Try to add same source again
        let result = add_source(&manager, &graph, source2).await;
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(
            format!("{error:#}").contains("component name already belongs to 'test-source'"),
            "unexpected duplicate-source error: {error:#}"
        );
    }

    #[tokio::test]
    async fn test_remove_source() {
        let (manager, graph) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string());
        add_source(&manager, &graph, source).await.unwrap();

        // Remove the source
        let result = delete_source(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
            false,
        )
        .await;
        assert!(result.is_ok());

        // Verify source was removed
        let sources = manager.list_sources().await.unwrap();
        assert_eq!(sources.len(), 0);
    }

    #[tokio::test]
    async fn test_remove_nonexistent_source() {
        let (manager, graph) = create_test_manager().await;

        let result = delete_source(
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
    async fn test_start_source() {
        let (manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.subscribe().unwrap();

        let source = create_test_mock_source("test-source".to_string());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager
            .get_source_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap()
            .id;

        // Start the source
        let result = manager
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_ok());

        // Check for status event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == source_id {
                    // Skip the Stopped event emitted by add_source (with "added" message)
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
    async fn test_stop_source() {
        let (manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.subscribe().unwrap();

        let source = create_test_mock_source("test-source".to_string());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager
            .get_source_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap()
            .id;
        manager
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();

        // Wait for source to reach Running before stopping
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("test-source").expect("fixture component name"),
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Stop the source
        let result = manager
            .stop_source_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_ok());

        // Check for stop event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Ok(event) = event_rx.recv().await {
                if event.component_id == source_id
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
    async fn test_stop_all_cleans_failed_source() {
        let (manager, graph) = create_test_manager().await;
        let source_name = drasi_core_types::ComponentName::try_new("failed-source")
            .expect("fixture component name");
        let source = LoggingTestSource::fixture("failed-source");
        add_source(&manager, &graph, source).await.unwrap();
        manager.start_source_by_name(&source_name).await.unwrap();
        let runtime = manager
            .get_source_instance_by_name(
                &drasi_core_types::ComponentName::try_new("failed-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        let source = runtime
            .as_any()
            .downcast_ref::<LoggingTestSource>()
            .unwrap();
        source.base.set_status(ComponentStatus::Error, None).await;
        let task = tokio::spawn(async {});
        let abort_handle = task.abort_handle();
        source.base.set_task_handle(task).await;
        let mut receiver = source.base.create_streaming_receiver().await.unwrap();
        crate::component_graph::wait_for_status_by_id(
            &graph,
            source.id(),
            &[ComponentStatus::Error],
            std::time::Duration::from_secs(3),
        )
        .await
        .unwrap();

        manager.stop_all().await.unwrap();
        assert!(abort_handle.is_finished());
        assert!(source.base.task_handle.read().await.is_none());
        crate::component_graph::wait_for_status_by_id(
            &graph,
            source.id(),
            &[ComponentStatus::Stopped],
            std::time::Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(
            manager
                .get_source_status_by_name(&source_name)
                .await
                .unwrap(),
            ComponentStatus::Stopped
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_stop_failed_source_with_already_stopped_runtime() {
        let (manager, graph) = create_test_manager().await;
        let source_name = drasi_core_types::ComponentName::try_new("failed-start")
            .expect("fixture component name");
        let source = LoggingTestSource::fixture("failed-start").map(|mut source| {
            source.fail_start = true;
            source
        });
        add_source(&manager, &graph, source).await.unwrap();
        assert!(manager.start_source_by_name(&source_name).await.is_err());
        crate::component_graph::wait_for_status_by_name(
            &graph,
            &source_name,
            &[ComponentStatus::Error],
            std::time::Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(
            manager
                .get_source_instance_by_name(
                    &drasi_core_types::ComponentName::try_new("failed-start")
                        .expect("fixture component name")
                )
                .await
                .unwrap()
                .status()
                .await,
            ComponentStatus::Stopped,
        );
        manager.stop_source_by_name(&source_name).await.unwrap();
        crate::component_graph::wait_for_status_by_name(
            &graph,
            &source_name,
            &[ComponentStatus::Stopped],
            std::time::Duration::from_secs(3),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_get_source_info() {
        let (manager, graph) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string());
        add_source(&manager, &graph, source).await.unwrap();

        let retrieved = manager
            .get_source_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(retrieved.is_ok());

        let retrieved = retrieved.unwrap();
        assert_ne!(
            retrieved.id,
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000001d")
                .expect("fixture UUID"),
            "source creation uses the graph-assigned component ID"
        );
        assert_eq!(retrieved.source_type, "mock");
    }

    #[tokio::test]
    async fn test_list_sources_with_status() {
        let (manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.subscribe().unwrap();

        // Add multiple sources
        let source1 = create_test_mock_source("source1".to_string());
        let source2 = create_test_mock_source("source2".to_string());

        add_source(&manager, &graph, source1).await.unwrap();
        add_source(&manager, &graph, source2).await.unwrap();

        // Start one source
        manager
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("source1")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();

        // Wait for source1 to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("source1").expect("fixture component name"),
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let sources = manager.list_sources().await.unwrap();
        assert_eq!(sources.len(), 2);

        // Check that we have different statuses
        let source1_status = sources
            .iter()
            .find(|entry| entry.name == crate::ComponentName::try_new("source1").unwrap())
            .unwrap()
            .status;
        let source2_status = sources
            .iter()
            .find(|entry| entry.name == crate::ComponentName::try_new("source2").unwrap())
            .unwrap()
            .status;

        assert!(matches!(source1_status, ComponentStatus::Running));
        assert!(matches!(source2_status, ComponentStatus::Added));
    }

    /// Test that concurrent add_source calls with the same ID are handled atomically.
    /// Only one should succeed, the others should fail with "already exists".
    /// This tests the TOCTOU fix where we use a single write lock for check-and-insert.
    #[tokio::test]
    async fn test_concurrent_add_source_same_id() {
        let (manager, graph) = create_test_manager().await;

        // Spawn multiple tasks trying to add a source with the same ID concurrently
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let graph_clone = graph.clone();
            handles.push(tokio::spawn(async move {
                let source = create_test_mock_source("same-source".to_string());
                let result = add_source(&manager_clone, &graph_clone, source).await;
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
        assert_eq!(success_count, 1, "Exactly one add_source should succeed");
        assert_eq!(failure_count, 9, "All other add_source calls should fail");

        // Verify only one source exists
        let sources = manager.list_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].name,
            drasi_core_types::ComponentName::try_new("same-source")
                .expect("fixture component name")
        );
    }

    /// Test that concurrent add_source calls with different IDs all succeed.
    #[tokio::test]
    async fn test_concurrent_add_source_different_ids() {
        let (manager, graph) = create_test_manager().await;

        // Spawn multiple tasks adding sources with unique IDs
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let graph_clone = graph.clone();
            handles.push(tokio::spawn(async move {
                let source = create_test_mock_source(format!("source-{i}"));
                add_source(&manager_clone, &graph_clone, source).await
            }));
        }

        // Wait for all tasks to complete
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(
                result.is_ok(),
                "All add_source calls with unique IDs should succeed"
            );
        }

        // Verify all 10 sources exist
        let sources = manager.list_sources().await.unwrap();
        assert_eq!(sources.len(), 10);
    }

    // ============================================================================
    // Auto-start tests
    // ============================================================================

    /// Test that start_all only starts sources with auto_start=true
    #[tokio::test]
    async fn test_start_all_respects_auto_start() {
        let (manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.subscribe().unwrap();

        // Add source with auto_start=true
        let source1 = TestMockSource::fixture("auto-start-source", true);
        add_source(&manager, &graph, source1).await.unwrap();

        // Add source with auto_start=false
        let source2 = TestMockSource::fixture("no-auto-start-source", false);
        add_source(&manager, &graph, source2).await.unwrap();

        // Start all sources
        manager.start_all().await.unwrap();

        // Wait for auto-start source to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("auto-start-source").expect("fixture component name"),
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        // Check statuses
        let status1 = manager
            .get_source_status_by_name(
                &drasi_core_types::ComponentName::try_new("auto-start-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        let status2 = manager
            .get_source_status_by_name(
                &drasi_core_types::ComponentName::try_new("no-auto-start-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();

        assert!(
            matches!(status1, ComponentStatus::Running),
            "Source with auto_start=true should be running"
        );
        assert!(
            matches!(status2, ComponentStatus::Added),
            "Source with auto_start=false should still be in Added state"
        );
    }

    /// Test that source auto_start defaults to true
    #[tokio::test]
    async fn test_source_auto_start_defaults_to_true() {
        let (manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.subscribe().unwrap();

        // Create source using default constructor (should have auto_start=true)
        let source = create_test_mock_source("default-source".to_string());

        // Verify auto_start is true
        assert!(source.auto_start(), "Default auto_start should be true");

        add_source(&manager, &graph, source).await.unwrap();

        // Start all should start this source
        manager.start_all().await.unwrap();

        // Wait for source to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("default-source").expect("fixture component name"),
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let status = manager
            .get_source_status_by_name(
                &drasi_core_types::ComponentName::try_new("default-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Running),
            "Default source should be started by start_all"
        );
    }

    /// Test that source with auto_start=false can be manually started
    #[tokio::test]
    async fn test_source_auto_start_false_can_be_manually_started() {
        let (manager, graph) = create_test_manager().await;

        // Subscribe to graph events BEFORE adding components
        let mut event_rx = graph.subscribe().unwrap();

        // Add source with auto_start=false
        let source = TestMockSource::fixture("manual-source", false);
        add_source(&manager, &graph, source).await.unwrap();

        // start_all should not start it
        manager.start_all().await.unwrap();

        // Yield to let any pending graph updates propagate
        tokio::task::yield_now().await;

        let status = manager
            .get_source_status_by_name(
                &drasi_core_types::ComponentName::try_new("manual-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Added),
            "Source with auto_start=false should not be started by start_all"
        );

        // Manually start the source
        manager
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("manual-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();

        // Wait for source to reach Running
        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &graph,
            &crate::ComponentName::try_new("manual-source").expect("fixture component name"),
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;

        let status = manager
            .get_source_status_by_name(
                &drasi_core_types::ComponentName::try_new("manual-source")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Running),
            "Source with auto_start=false should be manually startable"
        );
    }

    // ============================================================================
    // Log Streaming Integration Tests
    // ============================================================================

    #[tokio::test]
    async fn test_source_log_subscription() {
        let (manager, graph) = create_test_manager().await;

        // Create a mock source
        let source = create_test_mock_source("logging-source".to_string());
        add_source(&manager, &graph, source).await.unwrap();

        // Subscribe to logs
        let result = manager
            .subscribe_logs_by_name(
                &drasi_core_types::ComponentName::try_new("logging-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(
            result.is_ok(),
            "Should be able to subscribe to existing source logs"
        );

        let (history, _receiver) = result.unwrap();
        // History should be empty initially
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_source_log_subscription_nonexistent() {
        let (manager, _graph) = create_test_manager().await;

        // Try to subscribe to a non-existent source
        let result = manager
            .subscribe_logs_by_name(
                &drasi_core_types::ComponentName::try_new("nonexistent-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(
            result.is_err(),
            "Should return None for non-existent source"
        );
    }

    #[tokio::test]
    async fn test_source_base_logs_flow_to_subscriber() {
        use tracing::Instrument;

        let (manager, graph) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_name = unique_name("logger-source");

        // Create a LoggingTestSource that uses SourceBase
        let source = LoggingTestSource::fixture(source_name.as_str());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager.get_source_by_name(&source_name).await.unwrap().id;

        // Subscribe to logs before emitting
        let (history, mut receiver) = manager.subscribe_logs_by_name(&source_name).await.unwrap();
        assert!(history.is_empty(), "No logs should exist before logging");

        // Emit a log within a component span (must include instance_id to match manager)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("test log message");
        }
        .instrument(span)
        .await;

        // Yield to allow async log routing to complete
        tokio::task::yield_now().await;

        // Should receive the log via broadcast
        let received = tokio::time::timeout(std::time::Duration::from_millis(200), receiver.recv())
            .await
            .expect("Timeout waiting for log")
            .expect("Channel error");

        assert!(
            received.message.contains("test log message"),
            "Expected message containing 'test log message', got: {}",
            received.message
        );
        assert_eq!(received.component_id, source_id);
        assert_eq!(received.level, crate::observability::LogLevel::Info);
    }

    #[tokio::test]
    async fn test_source_base_logs_all_levels() {
        use tracing::Instrument;

        let (manager, graph) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_name = unique_name("multi-level-source");

        // Create a LoggingTestSource
        let source = LoggingTestSource::fixture(source_name.as_str());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager.get_source_by_name(&source_name).await.unwrap().id;

        // Emit logs at all levels within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::trace!("trace level");
            tracing::debug!("debug level");
            tracing::info!("info level");
            tracing::warn!("warn level");
            tracing::error!("error level");
        }
        .instrument(span)
        .await;

        // Allow async log routing to complete (logs go through tracing layer → channel → registry)
        let history = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok((logs, _)) = manager.subscribe_logs_by_name(&source_name).await {
                    if logs.len() >= 3 {
                        return logs;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Timed out waiting for logs to be routed");

        // Should have log messages (note: trace/debug may be filtered by EnvFilter)
        // The tracing subscriber uses INFO as default level
        assert!(
            history.len() >= 3,
            "Should have at least 3 log messages (info, warn, error)"
        );
    }

    #[tokio::test]
    async fn test_source_base_log_history_persists() {
        use tracing::Instrument;

        let (manager, graph) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_name = unique_name("history-source");

        // Create and add source
        let source = LoggingTestSource::fixture(source_name.as_str());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager.get_source_by_name(&source_name).await.unwrap().id;

        // Emit some logs within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("first");
            tracing::info!("second");
            tracing::info!("third");
        }
        .instrument(span)
        .await;

        // Allow async log routing to complete (logs go through tracing layer → channel → registry)
        let history = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok((logs, _)) = manager.subscribe_logs_by_name(&source_name).await {
                    if logs.len() >= 3 {
                        return logs;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Timed out waiting for logs to be routed");

        // Subscribe and verify history
        assert_eq!(history.len(), 3);
        assert!(history[0].message.contains("first"));
        assert!(history[1].message.contains("second"));
        assert!(history[2].message.contains("third"));

        // Subscribe again - should get same history
        let (history2, _receiver2) = manager.subscribe_logs_by_name(&source_name).await.unwrap();
        assert_eq!(history2.len(), 3);
    }

    // ============================================================================
    // Log Macro Routing Tests
    // ============================================================================

    #[tokio::test]
    async fn test_log_macro_routed_to_component_logs() {
        use tracing::Instrument;

        let (manager, graph) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_name = unique_name("log-routing-source");

        let source = LoggingTestSource::fixture(source_name.as_str());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager.get_source_by_name(&source_name).await.unwrap().id;

        let (_history, mut receiver) = manager.subscribe_logs_by_name(&source_name).await.unwrap();

        // Call tracing::info!() within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("Test log from macro");
        }
        .instrument(span)
        .await;

        // Yield to allow async log routing to complete
        tokio::task::yield_now().await;

        let received =
            tokio::time::timeout(std::time::Duration::from_millis(200), receiver.recv()).await;

        match received {
            Ok(Ok(msg)) => {
                assert!(
                    msg.message.contains("Test log from macro"),
                    "Expected message containing 'Test log from macro', got: {}",
                    msg.message
                );
            }
            Ok(Err(e)) => panic!("Channel error: {e:?}"),
            Err(_) => {
                let (history, _) = manager.subscribe_logs_by_name(&source_name).await.unwrap();
                panic!(
                    "Timeout. History: {:?}",
                    history.iter().map(|m| &m.message).collect::<Vec<_>>()
                );
            }
        }
    }

    // ============================================================================
    // Cleanup Tests
    // ============================================================================

    /// Test that deleting a source cleans up its event history
    #[tokio::test]
    async fn test_delete_source_cleans_up_event_history() {
        let (manager, graph) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_name = unique_name("cleanup-events-source");

        // Add a source
        let source = LoggingTestSource::fixture(source_name.as_str());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager.get_source_by_name(&source_name).await.unwrap().id;

        manager.start_source_by_name(&source_name).await.unwrap();

        // Verify events exist
        let events = manager
            .get_source_events_by_name(&source_name)
            .await
            .unwrap();
        assert!(!events.is_empty(), "Expected events after recording");

        // Delete the source
        delete_source(&manager, &graph, &source_name, false)
            .await
            .unwrap();

        // Verify events are cleaned up
        let events_after = manager.get_source_events_by_id(&source_id).await.unwrap();
        assert!(events_after.is_empty(), "Expected no events after deletion");
    }

    /// Test that deleting a source cleans up its log history
    #[tokio::test]
    async fn test_delete_source_cleans_up_log_history() {
        use tracing::Instrument;

        let (manager, graph) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_name = unique_name("cleanup-logs-source");

        // Add a source
        let source = LoggingTestSource::fixture(source_name.as_str());
        add_source(&manager, &graph, source).await.unwrap();
        let source_id = manager.get_source_by_name(&source_name).await.unwrap().id;

        // Generate some logs using tracing within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("test log message");
        }
        .instrument(span)
        .await;

        // Allow async log routing to complete (logs go through tracing layer → channel → registry)
        // Use a short retry loop since yield_now() alone may not be sufficient
        let logs_ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok((logs, _)) = manager.subscribe_logs_by_name(&source_name).await {
                    if !logs.is_empty() {
                        return logs;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Timed out waiting for logs to be routed");

        // Verify logs exist
        assert!(!logs_ready.is_empty(), "Expected logs after emitting");

        // Delete the source
        delete_source(&manager, &graph, &source_name, false)
            .await
            .unwrap();

        // Verify logs are cleaned up (subscribe should fail for non-existent source)
        let result = manager.subscribe_logs_by_name(&source_name).await;
        assert!(result.is_err(), "Expected an error for deleted source logs");
    }

    // ========================================================================
    // Deprovision tests
    // ========================================================================

    /// A test source that tracks deprovision calls.
    struct DeprovisionTestSource {
        id: drasi_core_types::ComponentId,
        status_handle: crate::component_graph::ComponentStatusHandle,
        deprovision_called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl DeprovisionTestSource {
        fn new(
            name: drasi_core_types::ComponentName,
        ) -> (
            crate::test_helpers::creation::SourceFixture<Self>,
            Arc<std::sync::atomic::AtomicBool>,
        ) {
            let deprovision_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let called = deprovision_called.clone();
            (
                crate::test_helpers::creation::SourceFixture::new(name, false, move |id| {
                    Ok(Self {
                        id: id.clone(),
                        status_handle: crate::component_graph::ComponentStatusHandle::new(
                            id.clone(),
                        ),
                        deprovision_called: called,
                    })
                }),
                deprovision_called,
            )
        }
    }

    #[async_trait]
    impl Source for DeprovisionTestSource {
        fn id(&self) -> &drasi_core_types::ComponentId {
            &self.id
        }

        fn type_name(&self) -> &str {
            "deprovision-test"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn auto_start(&self) -> bool {
            false
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

        async fn subscribe(
            &self,
            _settings: crate::config::SourceSubscriptionSettings,
        ) -> Result<SubscriptionResponse> {
            Err(anyhow::anyhow!("Not supported"))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn delete_owned_resources(&self) -> Result<()> {
            self.deprovision_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn bind_runtime_context(&self, context: crate::context::SourceRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }
    }

    #[tokio::test]
    async fn test_delete_with_cleanup_calls_deprovision() {
        let (manager, graph) = create_test_manager().await;

        let (source, deprovision_flag) = DeprovisionTestSource::new(
            drasi_core_types::ComponentName::try_new("deprovision-source")
                .expect("fixture component name"),
        );
        add_source(&manager, &graph, source).await.unwrap();

        // Delete with cleanup = true
        delete_source(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("deprovision-source")
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
    async fn test_delete_without_cleanup_skips_deprovision() {
        let (manager, graph) = create_test_manager().await;

        let (source, deprovision_flag) = DeprovisionTestSource::new(
            drasi_core_types::ComponentName::try_new("no-deprovision-source")
                .expect("fixture component name"),
        );
        add_source(&manager, &graph, source).await.unwrap();

        // Delete with cleanup = false
        delete_source(
            &manager,
            &graph,
            &drasi_core_types::ComponentName::try_new("no-deprovision-source")
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
    async fn test_update_source_replaces_stopped_source() {
        use crate::test_helpers::updates;
        let (core, trace) = updates::core().await;
        let source_name = drasi_core_types::ComponentName::try_new("reconfig-stopped-source")
            .expect("fixture component name");
        let old = updates::add(&core, source_name.clone(), false).await;
        core.source_manager()
            .update_source_by_name(
                &source_name,
                updates::definition(old.id().clone(), source_name.clone(), false, 1),
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
    async fn test_update_source_stops_and_restarts_running_source() {
        use crate::test_helpers::updates;
        let (core, trace) = updates::core().await;
        let source_name = drasi_core_types::ComponentName::try_new("reconfig-running-source")
            .expect("fixture component name");
        let old = updates::add(&core, source_name.clone(), false).await;
        old.start().await.unwrap();
        old.wait_started().await.unwrap();
        core.source_manager()
            .update_source_by_name(
                &source_name,
                updates::definition(old.id().clone(), source_name.clone(), false, 1),
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
            core.source_manager()
                .get_source_status_by_name(&source_name)
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
    async fn test_update_source_preserves_log_history() {
        use crate::test_helpers::updates;
        use tracing::Instrument;
        let (core, _) = updates::core().await;
        let source_name = drasi_core_types::ComponentName::try_new("reconfig-logs-source")
            .expect("fixture component name");
        let source = updates::add(&core, source_name.clone(), false).await;
        let source_id = source.id().clone();
        let manager = core.source_manager();

        // Generate some logs
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("pre-update log");
        }
        .instrument(span)
        .await;
        // Yield to allow async log routing to complete
        // Allow async log routing to complete (logs go through tracing layer → channel → registry)
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok((logs, _)) = manager.subscribe_logs_by_name(&source_name).await {
                    if !logs.is_empty() {
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Timed out waiting for logs to be routed");

        // Verify logs exist
        let (logs_before, _) = manager.subscribe_logs_by_name(&source_name).await.unwrap();
        assert!(!logs_before.is_empty(), "Expected logs before update");

        manager
            .update_source_by_name(
                &source_name,
                updates::definition(source.id().clone(), source_name.clone(), false, 1),
            )
            .await
            .unwrap();

        // Verify logs still exist after update
        let (logs_after, _) = manager.subscribe_logs_by_name(&source_name).await.unwrap();
        assert!(
            !logs_after.is_empty(),
            "Expected logs to be preserved after update"
        );
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_source_emits_reconfiguring_event() {
        use crate::test_helpers::updates;
        let (core, _) = updates::core().await;
        let source_name = drasi_core_types::ComponentName::try_new("reconfig-event-source")
            .expect("fixture component name");
        let source = updates::add(&core, source_name.clone(), false).await;
        source.start().await.unwrap();
        source.wait_started().await.unwrap();
        let mut event_rx = core.component_graph().subscribe().unwrap();
        core.source_manager()
            .update_source_by_name(
                &source_name,
                updates::definition(source.id().clone(), source_name.clone(), false, 1),
            )
            .await
            .unwrap();

        // Wait for Reconfiguring event via the broadcast channel
        wait_for_component_status_by_id(
            &mut event_rx,
            source.id(),
            ComponentStatus::Reconfiguring,
            std::time::Duration::from_secs(5),
        )
        .await;
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_update_source_rejects_mismatched_id() {
        use crate::test_helpers::updates;
        let (core, trace) = updates::core().await;
        let source_name = drasi_core_types::ComponentName::try_new("original-source")
            .expect("fixture component name");
        let _source = updates::add(&core, source_name.clone(), false).await;
        let before = trace.events();
        let result = core
            .source_manager()
            .update_source_by_name(
                &source_name,
                updates::definition(
                    crate::config::ComponentId::generate(),
                    source_name.clone(),
                    false,
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
    async fn test_update_nonexistent_source() {
        let (manager, _graph) = create_test_manager().await;
        let id = crate::config::ComponentId::generate();
        let source_name = drasi_core_types::ComponentName::try_new("nonexistent")
            .expect("fixture component name");
        let definition =
            crate::test_helpers::updates::definition(id.clone(), source_name.clone(), false, 1);
        let result = manager
            .update_source_by_name(&source_name, definition)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
