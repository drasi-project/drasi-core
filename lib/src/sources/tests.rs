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

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::sources::SourceManager;
    use tokio::sync::mpsc;

    async fn create_test_manager() -> (
        Arc<SourceManager>,
        mpsc::Receiver<ComponentEvent>,
        mpsc::Sender<ComponentEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(100);
        let manager = Arc::new(SourceManager::new(event_tx.clone()));
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
        let result = manager.delete_source("test-source".to_string()).await;
        assert!(result.is_ok());

        // Verify source was removed
        let sources = manager.list_sources().await;
        assert_eq!(sources.len(), 0);
    }

    #[tokio::test]
    async fn test_remove_nonexistent_source() {
        let (manager, _event_rx, _event_tx) = create_test_manager().await;

        let result = manager.delete_source("nonexistent".to_string()).await;
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
}
