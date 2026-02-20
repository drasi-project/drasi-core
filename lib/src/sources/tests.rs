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
use tokio::sync::RwLock;

/// A simple test mock source for unit testing.
///
/// This mock source supports event injection for testing data flow through queries.
pub struct TestMockSource {
    id: String,
    auto_start: bool,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    /// Dispatchers for sending events to subscribed queries
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
}

impl TestMockSource {
    pub fn new(id: String, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            id,
            auto_start: true,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            dispatchers: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Create a new test mock source with configurable auto_start
    pub fn with_auto_start(
        id: String,
        event_tx: ComponentEventSender,
        auto_start: bool,
    ) -> Result<Self> {
        Ok(Self {
            id,
            auto_start,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            dispatchers: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Inject an event into all subscribed queries.
    pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
        let dispatchers = self.dispatchers.read().await;
        let wrapper = SourceEventWrapper::new(
            self.id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
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
    fn id(&self) -> &str {
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
        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting source".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Source started".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping source".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Source stopped".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
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
            bootstrap_receiver: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, _context: crate::context::SourceRuntimeContext) {
        // TestMockSource already has event_tx from constructor, so we don't need to store it again
    }
}

/// Helper to create a TestMockSource instance
pub fn create_test_mock_source(id: String, event_tx: ComponentEventSender) -> TestMockSource {
    TestMockSource::new(id, event_tx).unwrap()
}

/// A test mock source that provides a bootstrap channel for testing the bootstrap gate.
///
/// Unlike `TestMockSource` (which returns `bootstrap_receiver: None`), this source
/// accepts a pre-created `BootstrapEventReceiver` and returns it from `subscribe()`,
/// allowing tests to control bootstrap timing.
pub struct TestBootstrapMockSource {
    id: String,
    auto_start: bool,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    bootstrap_rx: Arc<tokio::sync::Mutex<Option<BootstrapEventReceiver>>>,
}

impl TestBootstrapMockSource {
    pub fn new(
        id: String,
        event_tx: ComponentEventSender,
        bootstrap_rx: BootstrapEventReceiver,
    ) -> Result<Self> {
        Ok(Self {
            id,
            auto_start: true,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            dispatchers: Arc::new(RwLock::new(Vec::new())),
            bootstrap_rx: Arc::new(tokio::sync::Mutex::new(Some(bootstrap_rx))),
        })
    }
}

#[async_trait]
impl Source for TestBootstrapMockSource {
    fn id(&self) -> &str {
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
        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting source".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Source started".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping source".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Source stopped".to_string()),
        };
        let _ = self.event_tx.send(event).await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
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
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, _context: crate::context::SourceRuntimeContext) {
        // TestBootstrapMockSource already has event_tx from constructor
    }
}

/// Helper to create a TestBootstrapMockSource instance
pub fn create_test_bootstrap_mock_source(
    id: String,
    event_tx: ComponentEventSender,
    bootstrap_rx: BootstrapEventReceiver,
) -> TestBootstrapMockSource {
    TestBootstrapMockSource::new(id, event_tx, bootstrap_rx).unwrap()
}

/// A test source that uses SourceBase for logging integration tests.
///
/// This source uses the full SourceBase infrastructure including logger support.
pub struct LoggingTestSource {
    base: crate::sources::SourceBase,
}

impl LoggingTestSource {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let params = crate::sources::SourceBaseParams::new(id);
        let base = crate::sources::SourceBase::new(params)?;
        Ok(Self { base })
    }

    /// Log a message at info level (for testing)
    /// Note: With tracing refactor, logs should be emitted via tracing::info!()
    /// within a span that has component_id and component_type attributes
    pub async fn emit_log(&self, _message: &str) {
        // Logging is now done via tracing spans, not ComponentLogger
        // This method is kept for API compatibility but does nothing
    }

    /// Log messages at various levels (for testing)
    pub async fn emit_all_log_levels(&self) {
        // Logging is now done via tracing spans, not ComponentLogger
        // This method is kept for API compatibility but does nothing
    }
}

#[async_trait]
impl Source for LoggingTestSource {
    fn id(&self) -> &str {
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
        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;
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

    async fn initialize(&self, context: crate::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::sources::SourceManager;
    use tokio::sync::mpsc;

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counter for generating unique test IDs
    static TEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Generate a unique component ID for tests to avoid conflicts in parallel test runs
    fn unique_id(prefix: &str) -> String {
        let counter = TEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{prefix}-{counter}")
    }

    async fn create_test_manager() -> (
        Arc<SourceManager>,
        mpsc::Receiver<ComponentEvent>,
        mpsc::Sender<ComponentEvent>,
    ) {
        // Use the global shared log registry since tracing subscriber is global
        let log_registry = crate::managers::get_or_init_global_registry();

        let (event_tx, event_rx) = mpsc::channel(100);
        let manager = Arc::new(SourceManager::new(
            "test-instance",
            event_tx.clone(),
            log_registry,
        ));
        (manager, event_rx, event_tx)
    }

    #[tokio::test]
    async fn test_add_source() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string(), event_tx);
        let result = manager.add_source(source).await;

        assert!(result.is_ok());

        // Verify source was added
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "test-source");
    }

    #[tokio::test]
    async fn test_add_duplicate_source() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let source1 = create_test_mock_source("test-source".to_string(), event_tx.clone());
        let source2 = create_test_mock_source("test-source".to_string(), event_tx);

        // Add source first time
        assert!(manager.add_source(source1).await.is_ok());

        // Try to add same source again
        let result = manager.add_source(source2).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_remove_source() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string(), event_tx);
        manager.add_source(source).await.unwrap();

        // Remove the source
        let result = manager
            .delete_source("test-source".to_string(), false)
            .await;
        assert!(result.is_ok());

        // Verify source was removed
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 0);
    }

    #[tokio::test]
    async fn test_remove_nonexistent_source() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let result = manager
            .delete_source("nonexistent".to_string(), false)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_start_source() {
        let (manager, mut event_rx, event_tx) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string(), event_tx);
        manager.add_source(source).await.unwrap();

        // Start the source
        let result = manager.start_source("test-source".to_string()).await;
        assert!(result.is_ok());

        // Check for status event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-source" {
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
        let (manager, mut event_rx, event_tx) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string(), event_tx);
        manager.add_source(source).await.unwrap();
        manager
            .start_source("test-source".to_string())
            .await
            .unwrap();

        // Wait a bit for source to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Stop the source
        let result = manager.stop_source("test-source".to_string()).await;
        assert!(result.is_ok());

        // Check for stop event
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(event) = event_rx.recv().await {
                if event.component_id == "test-source"
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
    async fn test_get_source_info() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        let source = create_test_mock_source("test-source".to_string(), event_tx);
        manager.add_source(source).await.unwrap();

        let retrieved = manager.get_source("test-source".to_string()).await;
        assert!(retrieved.is_ok());

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "test-source");
        assert_eq!(retrieved.source_type, "mock");
    }

    #[tokio::test]
    async fn test_list_sources_with_status() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add multiple sources
        let source1 = create_test_mock_source("source1".to_string(), event_tx.clone());
        let source2 = create_test_mock_source("source2".to_string(), event_tx);

        manager.add_source(source1).await.unwrap();
        manager.add_source(source2).await.unwrap();

        // Start one source
        manager.start_source("source1".to_string()).await.unwrap();

        // Wait a bit
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 2);

        // Check that we have different statuses
        let source1_status = sources
            .iter()
            .find(|(name, _)| name == "source1")
            .unwrap()
            .1
            .clone();
        let source2_status = sources
            .iter()
            .find(|(name, _)| name == "source2")
            .unwrap()
            .1
            .clone();

        assert!(matches!(source1_status, ComponentStatus::Running));
        assert!(matches!(source2_status, ComponentStatus::Stopped));
    }

    /// Test that concurrent add_source calls with the same ID are handled atomically.
    /// Only one should succeed, the others should fail with "already exists".
    /// This tests the TOCTOU fix where we use a single write lock for check-and-insert.
    #[tokio::test]
    async fn test_concurrent_add_source_same_id() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Spawn multiple tasks trying to add a source with the same ID concurrently
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let event_tx_clone = event_tx.clone();
            handles.push(tokio::spawn(async move {
                let source = create_test_mock_source("same-source".to_string(), event_tx_clone);
                let result = manager_clone.add_source(source).await;
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
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "same-source");
    }

    /// Test that concurrent add_source calls with different IDs all succeed.
    #[tokio::test]
    async fn test_concurrent_add_source_different_ids() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Spawn multiple tasks adding sources with unique IDs
        let mut handles = Vec::new();
        for i in 0..10 {
            let manager_clone = manager.clone();
            let event_tx_clone = event_tx.clone();
            handles.push(tokio::spawn(async move {
                let source = create_test_mock_source(format!("source-{i}"), event_tx_clone);
                manager_clone.add_source(source).await
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
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 10);
    }

    // ============================================================================
    // Auto-start tests
    // ============================================================================

    /// Test that start_all only starts sources with auto_start=true
    #[tokio::test]
    async fn test_start_all_respects_auto_start() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add source with auto_start=true
        let source1 = TestMockSource::with_auto_start(
            "auto-start-source".to_string(),
            event_tx.clone(),
            true,
        )
        .unwrap();
        manager.add_source(source1).await.unwrap();

        // Add source with auto_start=false
        let source2 = TestMockSource::with_auto_start(
            "no-auto-start-source".to_string(),
            event_tx.clone(),
            false,
        )
        .unwrap();
        manager.add_source(source2).await.unwrap();

        // Start all sources
        manager.start_all().await.unwrap();

        // Wait a bit for status to update
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Check statuses
        let status1 = manager
            .get_source_status("auto-start-source".to_string())
            .await
            .unwrap();
        let status2 = manager
            .get_source_status("no-auto-start-source".to_string())
            .await
            .unwrap();

        assert!(
            matches!(status1, ComponentStatus::Running),
            "Source with auto_start=true should be running"
        );
        assert!(
            matches!(status2, ComponentStatus::Stopped),
            "Source with auto_start=false should still be stopped"
        );
    }

    /// Test that source auto_start defaults to true
    #[tokio::test]
    async fn test_source_auto_start_defaults_to_true() {
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Create source using default constructor (should have auto_start=true)
        let source = create_test_mock_source("default-source".to_string(), event_tx);

        // Verify auto_start is true
        assert!(source.auto_start(), "Default auto_start should be true");

        manager.add_source(source).await.unwrap();

        // Start all should start this source
        manager.start_all().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let status = manager
            .get_source_status("default-source".to_string())
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
        let (manager, _event_rx, event_tx) = create_test_manager().await;

        // Add source with auto_start=false
        let source =
            TestMockSource::with_auto_start("manual-source".to_string(), event_tx, false).unwrap();
        manager.add_source(source).await.unwrap();

        // start_all should not start it
        manager.start_all().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let status = manager
            .get_source_status("manual-source".to_string())
            .await
            .unwrap();
        assert!(
            matches!(status, ComponentStatus::Stopped),
            "Source with auto_start=false should not be started by start_all"
        );

        // Manually start the source
        manager
            .start_source("manual-source".to_string())
            .await
            .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let status = manager
            .get_source_status("manual-source".to_string())
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
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Create a mock source
        let source = create_test_mock_source("logging-source".to_string(), _event_tx);
        manager.add_source(source).await.unwrap();

        // Subscribe to logs
        let result = manager.subscribe_logs("logging-source").await;
        assert!(
            result.is_some(),
            "Should be able to subscribe to existing source logs"
        );

        let (history, _receiver) = result.unwrap();
        // History should be empty initially
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_source_log_subscription_nonexistent() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Try to subscribe to a non-existent source
        let result = manager.subscribe_logs("nonexistent-source").await;
        assert!(
            result.is_none(),
            "Should return None for non-existent source"
        );
    }

    #[tokio::test]
    async fn test_source_base_logs_flow_to_subscriber() {
        use tracing::Instrument;

        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_id = unique_id("logger-source");
        let source_id_clone = source_id.clone();

        // Create a LoggingTestSource that uses SourceBase
        let source = LoggingTestSource::new(&source_id).unwrap();
        manager.add_source(source).await.unwrap();

        // Subscribe to logs before emitting
        let (history, mut receiver) = manager.subscribe_logs(&source_id).await.unwrap();
        assert!(history.is_empty(), "No logs should exist before logging");

        // Emit a log within a component span (must include instance_id to match manager)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id_clone,
            component_type = "source"
        );
        async {
            tracing::info!("test log message");
        }
        .instrument(span)
        .await;

        // Wait for async log routing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

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
        assert_eq!(received.level, crate::managers::LogLevel::Info);
    }

    #[tokio::test]
    async fn test_source_base_logs_all_levels() {
        use tracing::Instrument;

        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_id = unique_id("multi-level-source");
        let source_id_clone = source_id.clone();

        // Create a LoggingTestSource
        let source = LoggingTestSource::new(&source_id).unwrap();
        manager.add_source(source).await.unwrap();

        // Emit logs at all levels within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id_clone,
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

        // Wait for async log routing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Subscribe after logging to get history
        let (history, _receiver) = manager.subscribe_logs(&source_id).await.unwrap();

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

        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_id = unique_id("history-source");
        let source_id_clone = source_id.clone();

        // Create and add source
        let source = LoggingTestSource::new(&source_id).unwrap();
        manager.add_source(source).await.unwrap();

        // Emit some logs within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id_clone,
            component_type = "source"
        );
        async {
            tracing::info!("first");
            tracing::info!("second");
            tracing::info!("third");
        }
        .instrument(span)
        .await;

        // Wait for async log routing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Subscribe and verify history
        let (history, _receiver) = manager.subscribe_logs(&source_id).await.unwrap();
        assert_eq!(history.len(), 3);
        assert!(history[0].message.contains("first"));
        assert!(history[1].message.contains("second"));
        assert!(history[2].message.contains("third"));

        // Subscribe again - should get same history
        let (history2, _receiver2) = manager.subscribe_logs(&source_id).await.unwrap();
        assert_eq!(history2.len(), 3);
    }

    // ============================================================================
    // Log Macro Routing Tests
    // ============================================================================

    #[tokio::test]
    async fn test_log_macro_routed_to_component_logs() {
        use tracing::Instrument;

        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_id = unique_id("log-routing-source");
        let source_id_clone = source_id.clone();

        let source = LoggingTestSource::new(&source_id).unwrap();
        manager.add_source(source).await.unwrap();

        let (_history, mut receiver) = manager.subscribe_logs(&source_id).await.unwrap();

        // Call tracing::info!() within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id_clone,
            component_type = "source"
        );
        async {
            tracing::info!("Test log from macro");
        }
        .instrument(span)
        .await;

        // Wait for async log task
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

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
                let (history, _) = manager.subscribe_logs(&source_id).await.unwrap();
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
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_id = unique_id("cleanup-events-source");

        // Add a source
        let source = LoggingTestSource::new(&source_id).unwrap();
        manager.add_source(source).await.unwrap();

        // Record an event manually to simulate lifecycle
        manager
            .record_event(ComponentEvent {
                component_id: source_id.clone(),
                component_type: crate::ComponentType::Source,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("Test event".to_string()),
            })
            .await;

        // Verify events exist
        let events = manager.get_source_events(&source_id).await;
        assert!(!events.is_empty(), "Expected events after recording");

        // Delete the source
        manager
            .delete_source(source_id.clone(), false)
            .await
            .unwrap();

        // Verify events are cleaned up
        let events_after = manager.get_source_events(&source_id).await;
        assert!(events_after.is_empty(), "Expected no events after deletion");
    }

    /// Test that deleting a source cleans up its log history
    #[tokio::test]
    async fn test_delete_source_cleans_up_log_history() {
        use tracing::Instrument;

        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        // Use unique ID to avoid conflicts with parallel tests
        let source_id = unique_id("cleanup-logs-source");
        let source_id_clone = source_id.clone();

        // Add a source
        let source = LoggingTestSource::new(&source_id).unwrap();
        manager.add_source(source).await.unwrap();

        // Generate some logs using tracing within a component span (must include instance_id)
        let span = tracing::info_span!(
            "test_span",
            instance_id = "test-instance",
            component_id = %source_id_clone,
            component_type = "source"
        );
        async {
            tracing::info!("test log message");
        }
        .instrument(span)
        .await;

        // Wait for log to be recorded
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify logs exist
        let result = manager.subscribe_logs(&source_id).await;
        assert!(result.is_some(), "Expected to subscribe to source logs");
        let (logs, _) = result.unwrap();
        assert!(!logs.is_empty(), "Expected logs after emitting");

        // Delete the source
        manager
            .delete_source(source_id.clone(), false)
            .await
            .unwrap();

        // Verify logs are cleaned up (subscribe should fail for non-existent source)
        let result = manager.subscribe_logs(&source_id).await;
        assert!(result.is_none(), "Expected None for deleted source logs");
    }

    // ========================================================================
    // Deprovision tests
    // ========================================================================

    /// A test source that tracks deprovision calls.
    struct DeprovisionTestSource {
        id: String,
        status: Arc<RwLock<ComponentStatus>>,
        deprovision_called: Arc<std::sync::atomic::AtomicBool>,
    }

    impl DeprovisionTestSource {
        fn new(id: &str) -> (Self, Arc<std::sync::atomic::AtomicBool>) {
            let deprovision_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            (
                Self {
                    id: id.to_string(),
                    status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
                    deprovision_called: deprovision_called.clone(),
                },
                deprovision_called,
            )
        }

        fn new_simple(id: &str) -> Self {
            Self {
                id: id.to_string(),
                status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
                deprovision_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    #[async_trait]
    impl Source for DeprovisionTestSource {
        fn id(&self) -> &str {
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

        async fn subscribe(
            &self,
            _settings: crate::config::SourceSubscriptionSettings,
        ) -> Result<SubscriptionResponse> {
            Err(anyhow::anyhow!("Not supported"))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn deprovision(&self) -> Result<()> {
            self.deprovision_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn initialize(&self, _context: crate::context::SourceRuntimeContext) {}
    }

    #[tokio::test]
    async fn test_delete_with_cleanup_calls_deprovision() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let (source, deprovision_flag) = DeprovisionTestSource::new("deprovision-source");
        manager.add_source(source).await.unwrap();

        // Delete with cleanup = true
        manager
            .delete_source("deprovision-source".to_string(), true)
            .await
            .unwrap();

        assert!(
            deprovision_flag.load(std::sync::atomic::Ordering::SeqCst),
            "deprovision() should have been called"
        );
    }

    #[tokio::test]
    async fn test_delete_without_cleanup_skips_deprovision() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let (source, deprovision_flag) = DeprovisionTestSource::new("no-deprovision-source");
        manager.add_source(source).await.unwrap();

        // Delete with cleanup = false
        manager
            .delete_source("no-deprovision-source".to_string(), false)
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
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let source = DeprovisionTestSource::new_simple("reconfig-stopped-source");
        manager.add_source(source).await.unwrap();

        // Update while stopped by providing a new instance
        let new_source = DeprovisionTestSource::new_simple("reconfig-stopped-source");
        manager
            .update_source("reconfig-stopped-source".to_string(), new_source)
            .await
            .unwrap();

        // Source should still be stopped
        let status = manager
            .get_source_status("reconfig-stopped-source".to_string())
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_update_source_stops_and_restarts_running_source() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let source = DeprovisionTestSource::new_simple("reconfig-running-source");
        manager.add_source(source).await.unwrap();

        // Start the source first
        manager
            .start_source("reconfig-running-source".to_string())
            .await
            .unwrap();
        let status = manager
            .get_source_status("reconfig-running-source".to_string())
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Update while running
        let new_source = DeprovisionTestSource::new_simple("reconfig-running-source");
        manager
            .update_source("reconfig-running-source".to_string(), new_source)
            .await
            .unwrap();

        // Should be running again
        let status = manager
            .get_source_status("reconfig-running-source".to_string())
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_update_source_preserves_log_history() {
        use tracing::Instrument;

        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let source_id = unique_id("reconfig-logs-source");
        let source = DeprovisionTestSource::new_simple(&source_id);
        manager.add_source(source).await.unwrap();

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
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Verify logs exist
        let (logs_before, _) = manager.subscribe_logs(&source_id).await.unwrap();
        assert!(!logs_before.is_empty(), "Expected logs before update");

        // Update with a new instance
        let new_source = DeprovisionTestSource::new_simple(&source_id);
        manager
            .update_source(source_id.clone(), new_source)
            .await
            .unwrap();

        // Verify logs still exist after update
        let (logs_after, _) = manager.subscribe_logs(&source_id).await.unwrap();
        assert!(
            !logs_after.is_empty(),
            "Expected logs to be preserved after update"
        );
    }

    #[tokio::test]
    async fn test_update_source_emits_reconfiguring_event() {
        let (manager, mut event_rx, _event_tx) = create_test_manager().await;

        let source = DeprovisionTestSource::new_simple("reconfig-event-source");
        manager.add_source(source).await.unwrap();

        // Update
        let new_source = DeprovisionTestSource::new_simple("reconfig-event-source");
        manager
            .update_source("reconfig-event-source".to_string(), new_source)
            .await
            .unwrap();

        // Check events
        let mut found_reconfiguring = false;
        while let Ok(event) = event_rx.try_recv() {
            if event.component_id == "reconfig-event-source"
                && event.status == ComponentStatus::Reconfiguring
            {
                found_reconfiguring = true;
            }
        }
        assert!(found_reconfiguring, "Expected Reconfiguring event");
    }

    #[tokio::test]
    async fn test_update_source_rejects_mismatched_id() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let source = DeprovisionTestSource::new_simple("original-source");
        manager.add_source(source).await.unwrap();

        // Try to update with a different ID
        let new_source = DeprovisionTestSource::new_simple("different-id");
        let result = manager
            .update_source("original-source".to_string(), new_source)
            .await;
        assert!(result.is_err(), "Expected error for mismatched IDs");
        assert!(result.unwrap_err().to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn test_update_nonexistent_source() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let new_source = DeprovisionTestSource::new_simple("nonexistent");
        let result = manager
            .update_source("nonexistent".to_string(), new_source)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
