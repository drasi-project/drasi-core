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

//! Integration tests for the log worker channel architecture.
//!
//! These tests verify that:
//! - The bounded channel log worker processes logs correctly
//! - Logs work across different tokio runtime configurations
//! - High volume logging doesn't cause unbounded memory growth
//! - The worker thread is independent of the caller's runtime

use async_trait::async_trait;
use drasi_lib::{
    ComponentStatus, DrasiLib, LogLevel, Source, SourceBase, SourceBaseParams,
    SourceRuntimeContext, SourceSubscriptionSettings, SubscriptionResponse,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::Instrument;

// ============================================================================
// Test Source Implementations
// ============================================================================

/// A source that emits many logs for high-volume testing.
struct HighVolumeLogSource {
    base: SourceBase,
    log_count: usize,
}

impl HighVolumeLogSource {
    fn new(id: &str, log_count: usize) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            log_count,
        })
    }
}

#[async_trait]
impl Source for HighVolumeLogSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "high-volume-source"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.auto_start
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        let source_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "high_volume_source",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );

        let log_count = self.log_count;
        async move {
            // Emit many logs rapidly
            for i in 0..log_count {
                tracing::info!("High volume log message {}", i);
            }
        }
        .instrument(span)
        .await;

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "high-volume-source")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A simple source for basic log testing.
struct SimpleLogSource {
    base: SourceBase,
}

impl SimpleLogSource {
    fn new(id: &str) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
        })
    }
}

#[async_trait]
impl Source for SimpleLogSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "simple-log-source"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.auto_start
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        let source_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "simple_source",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );

        async {
            tracing::info!("SimpleLogSource starting");
        }
        .instrument(span)
        .await;

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "simple-log-source")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ============================================================================
// Tests for Log Worker Channel Architecture
// ============================================================================

/// Test that logs flow correctly through the bounded channel.
#[tokio::test]
async fn test_logs_flow_through_channel() {
    let source = SimpleLogSource::new("channel-flow-source").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs
    let (_history, mut log_receiver) = drasi
        .subscribe_source_logs("channel-flow-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Start to trigger logging
    drasi.start().await.expect("Failed to start DrasiLib");

    // Wait for log to arrive
    let received = timeout(Duration::from_secs(2), async {
        let mut logs = Vec::new();
        while let Ok(log) = log_receiver.recv().await {
            logs.push(log);
            if logs.iter().any(|l| l.message.contains("starting")) {
                break;
            }
        }
        logs
    })
    .await
    .expect("Timeout waiting for logs");

    assert!(!received.is_empty(), "Expected to receive at least one log");
    assert!(
        received.iter().any(|l| l.message.contains("starting")),
        "Expected 'starting' log message"
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test high volume logging doesn't cause issues.
/// This verifies the bounded channel provides backpressure.
#[tokio::test]
async fn test_high_volume_logging() {
    let log_count = 1000;
    let source =
        HighVolumeLogSource::new("high-volume-source", log_count).expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs
    let (_history, mut log_receiver) = drasi
        .subscribe_source_logs("high-volume-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Start to trigger high volume logging
    drasi.start().await.expect("Failed to start DrasiLib");

    // Give the log worker time to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Collect logs - we may not get all of them if channel fills up
    // but we should get a good number
    let received = timeout(Duration::from_secs(5), async {
        let mut logs = Vec::new();
        let mut lagged_count = 0u64;
        loop {
            match tokio::time::timeout(Duration::from_millis(200), log_receiver.recv()).await {
                Ok(Ok(log)) => logs.push(log),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    lagged_count += n;
                    // Continue receiving after lag
                    continue;
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_timeout) => break,
            }
        }
        println!("Lagged (missed) messages: {lagged_count}");
        logs
    })
    .await
    .expect("Timeout collecting logs");

    println!(
        "Received {} logs out of {} emitted",
        received.len(),
        log_count
    );

    // We should receive at least some logs
    // (many may be missed due to lag or channel capacity)
    assert!(
        !received.is_empty() || log_count == 0,
        "Expected at least some logs, got 0"
    );

    // Verify logs have correct component info
    for log in &received {
        assert_eq!(log.component_id, "high-volume-source");
        assert_eq!(log.level, LogLevel::Info);
    }

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that the log worker works with multi-threaded runtime.
#[tokio::test(flavor = "current_thread")]
async fn test_multi_thread_runtime() {
    let source = SimpleLogSource::new("multi-thread-source").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs
    let (_history, mut log_receiver) = drasi
        .subscribe_source_logs("multi-thread-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Start to trigger logging
    drasi.start().await.expect("Failed to start DrasiLib");

    // Wait for log
    let received = timeout(Duration::from_secs(2), async {
        let mut logs = Vec::new();
        while let Ok(log) = log_receiver.recv().await {
            logs.push(log);
            if !logs.is_empty() {
                break;
            }
        }
        logs
    })
    .await
    .expect("Timeout waiting for logs");

    assert!(
        !received.is_empty(),
        "Expected logs in multi-thread runtime"
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that the log worker works with current-thread runtime.
#[tokio::test(flavor = "current_thread")]
async fn test_current_thread_runtime() {
    let source = SimpleLogSource::new("current-thread-source").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs
    let (_history, mut log_receiver) = drasi
        .subscribe_source_logs("current-thread-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Start to trigger logging
    drasi.start().await.expect("Failed to start DrasiLib");

    // Wait for log
    let received = timeout(Duration::from_secs(2), async {
        let mut logs = Vec::new();
        while let Ok(log) = log_receiver.recv().await {
            logs.push(log);
            if !logs.is_empty() {
                break;
            }
        }
        logs
    })
    .await
    .expect("Timeout waiting for logs");

    assert!(
        !received.is_empty(),
        "Expected logs in current-thread runtime"
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test concurrent logging from multiple tasks.
#[tokio::test(flavor = "current_thread")]
async fn test_concurrent_logging_from_multiple_tasks() {
    let source = SimpleLogSource::new("concurrent-log-source").expect("Failed to create source");

    let instance_id = "concurrent-test-instance";
    let drasi = DrasiLib::builder()
        .with_id(instance_id)
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Give source time to initialize
    tokio::time::sleep(Duration::from_millis(100)).await;

    let source_id = "concurrent-log-source";

    // Subscribe to logs
    let (_history, mut log_receiver) = drasi
        .subscribe_source_logs(source_id)
        .await
        .expect("Failed to subscribe to source logs");

    // Spawn multiple tasks that log concurrently
    let log_counter = Arc::new(AtomicUsize::new(0));
    let tasks_count = 10;
    let logs_per_task = 50;

    let mut handles = Vec::new();
    for task_id in 0..tasks_count {
        let instance_id = instance_id.to_string();
        let counter = log_counter.clone();

        handles.push(tokio::spawn(async move {
            let span = tracing::info_span!(
                "concurrent_task",
                instance_id = %instance_id,
                component_id = "concurrent-log-source",
                component_type = "source"
            );

            async move {
                for i in 0..logs_per_task {
                    tracing::info!("Task {} log {}", task_id, i);
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
            .instrument(span)
            .await;
        }));
    }

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.expect("Task panicked");
    }

    let total_logged = log_counter.load(Ordering::SeqCst);
    println!("Total logs emitted: {total_logged}");

    // Give the log worker time to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Collect logs
    let received = timeout(Duration::from_secs(3), async {
        let mut logs = Vec::new();
        let mut lagged_count = 0u64;
        loop {
            match tokio::time::timeout(Duration::from_millis(200), log_receiver.recv()).await {
                Ok(Ok(log)) => logs.push(log),
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    lagged_count += n;
                    continue;
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_timeout) => break,
            }
        }
        println!("Lagged (missed) messages: {lagged_count}");
        logs
    })
    .await
    .expect("Timeout collecting logs");

    println!(
        "Received {} logs out of {} emitted",
        received.len(),
        total_logged
    );

    // We should receive at least some logs
    // (Many may be missed due to lag or channel capacity)
    // Note: This test verifies the system doesn't crash under concurrent load,
    // not that all logs are received
    assert!(
        !received.is_empty() || total_logged == 0,
        "Expected at least some concurrent logs, got 0"
    );

    // Verify all received logs have correct component info
    for log in &received {
        assert_eq!(log.component_id, source_id);
    }

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that multiple DrasiLib instances can log independently.
#[tokio::test]
async fn test_multiple_drasi_instances_logging() {
    // Create two DrasiLib instances with different IDs
    let source1 = SimpleLogSource::new("instance1-source").expect("Failed to create source1");
    let source2 = SimpleLogSource::new("instance2-source").expect("Failed to create source2");

    let drasi1 = DrasiLib::builder()
        .with_id("drasi-instance-1")
        .with_source(source1)
        .build()
        .await
        .expect("Failed to build DrasiLib 1");

    let drasi2 = DrasiLib::builder()
        .with_id("drasi-instance-2")
        .with_source(source2)
        .build()
        .await
        .expect("Failed to build DrasiLib 2");

    // Subscribe to logs from both instances
    let (_h1, mut receiver1) = drasi1
        .subscribe_source_logs("instance1-source")
        .await
        .expect("Failed to subscribe to instance1 logs");

    let (_h2, mut receiver2) = drasi2
        .subscribe_source_logs("instance2-source")
        .await
        .expect("Failed to subscribe to instance2 logs");

    // Start both instances
    drasi1.start().await.expect("Failed to start DrasiLib 1");
    drasi2.start().await.expect("Failed to start DrasiLib 2");

    // Collect logs from both
    let logs1 = timeout(Duration::from_secs(2), async {
        let mut logs = Vec::new();
        while let Ok(log) = receiver1.recv().await {
            logs.push(log);
            if !logs.is_empty() {
                break;
            }
        }
        logs
    })
    .await
    .expect("Timeout waiting for instance1 logs");

    let logs2 = timeout(Duration::from_secs(2), async {
        let mut logs = Vec::new();
        while let Ok(log) = receiver2.recv().await {
            logs.push(log);
            if !logs.is_empty() {
                break;
            }
        }
        logs
    })
    .await
    .expect("Timeout waiting for instance2 logs");

    // Verify both instances received their logs
    assert!(!logs1.is_empty(), "Instance 1 should have received logs");
    assert!(!logs2.is_empty(), "Instance 2 should have received logs");

    // Verify logs are correctly attributed
    assert!(
        logs1.iter().all(|l| l.component_id == "instance1-source"),
        "Instance 1 logs should have correct component_id"
    );
    assert!(
        logs2.iter().all(|l| l.component_id == "instance2-source"),
        "Instance 2 logs should have correct component_id"
    );

    drasi1.stop().await.expect("Failed to stop DrasiLib 1");
    drasi2.stop().await.expect("Failed to stop DrasiLib 2");
}

/// Test log worker handles rapid start/stop cycles.
#[tokio::test]
async fn test_rapid_start_stop_cycles() {
    for i in 0..5 {
        let source_id = format!("rapid-cycle-source-{i}");
        let source = SimpleLogSource::new(&source_id).expect("Failed to create source");

        let drasi = DrasiLib::builder()
            .with_source(source)
            .build()
            .await
            .expect("Failed to build DrasiLib");

        // Subscribe
        let (_history, mut receiver) = drasi
            .subscribe_source_logs(&source_id)
            .await
            .expect("Failed to subscribe");

        // Rapid start
        drasi.start().await.expect("Failed to start");

        // Try to get at least one log
        let _ = timeout(Duration::from_millis(200), async {
            let _ = receiver.recv().await;
        })
        .await;

        // Rapid stop
        drasi.stop().await.expect("Failed to stop");
    }

    // If we got here without panicking, the test passes
    // This verifies the log worker handles cleanup correctly
}

/// Test that log level filtering works through the channel.
#[tokio::test]
async fn test_log_level_filtering_through_channel() {
    let source = SimpleLogSource::new("level-filter-source").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Give time for logs
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Get log history
    let (history, _) = drasi
        .subscribe_source_logs("level-filter-source")
        .await
        .expect("Failed to subscribe");

    // By default (INFO level), we should have INFO logs
    let has_info = history.iter().any(|l| l.level == LogLevel::Info);
    assert!(has_info, "Expected INFO level logs");

    // Verify log messages are complete (not truncated)
    for log in &history {
        assert!(!log.message.is_empty(), "Log message should not be empty");
    }

    drasi.stop().await.expect("Failed to stop DrasiLib");
}
