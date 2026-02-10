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

//! Integration tests for the deprovision and reconfigure features.
//!
//! These tests exercise the full DrasiLib public API for:
//! - `remove_source(id, cleanup: bool)` / `remove_reaction(id, cleanup: bool)`
//! - `update_source(id, new_source_instance)` / `update_reaction(id, new_reaction_instance)`
//! - Lifecycle event streaming during deprovision and reconfigure operations
//! - Log history preservation across reconfigure cycles

use async_trait::async_trait;
use drasi_lib::{
    ComponentEvent, ComponentStatus, DrasiLib, MemoryStateStoreProvider, Query, Reaction,
    ReactionRuntimeContext, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
    SourceSubscriptionSettings, StateStoreProvider, SubscriptionResponse,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::Instrument;

// ============================================================================
// Test Source with deprovision/reconfigure support
// ============================================================================

struct TestSource {
    base: SourceBase,
    deprovision_called: Arc<AtomicBool>,
    start_count: Arc<AtomicU32>,
    stop_count: Arc<AtomicU32>,
}

impl TestSource {
    fn new(id: &str) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            deprovision_called: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            stop_count: Arc::new(AtomicU32::new(0)),
        })
    }

    fn deprovision_called(&self) -> Arc<AtomicBool> {
        self.deprovision_called.clone()
    }

    fn start_count(&self) -> Arc<AtomicU32> {
        self.start_count.clone()
    }

    fn stop_count(&self) -> Arc<AtomicU32> {
        self.stop_count.clone()
    }
}

#[async_trait]
impl Source for TestSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "deprovision-test-source"
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
        self.start_count.fetch_add(1, Ordering::SeqCst);
        let source_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_source_start",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("TestSource starting");
        }
        .instrument(span)
        .await;

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.stop_count.fetch_add(1, Ordering::SeqCst);
        let source_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_source_stop",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("TestSource stopping");
        }
        .instrument(span)
        .await;

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
            .subscribe_with_bootstrap(&settings, "test-source")
            .await
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        self.deprovision_called.store(true, Ordering::SeqCst);
        self.base.deprovision_common().await?;
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ============================================================================
// Test Reaction with deprovision/reconfigure support
// ============================================================================

struct TestReaction {
    base: drasi_lib::ReactionBase,
    deprovision_called: Arc<AtomicBool>,
    start_count: Arc<AtomicU32>,
    stop_count: Arc<AtomicU32>,
}

impl TestReaction {
    fn new(id: &str, query_ids: Vec<String>) -> Self {
        let params = drasi_lib::ReactionBaseParams::new(id, query_ids);
        Self {
            base: drasi_lib::ReactionBase::new(params),
            deprovision_called: Arc::new(AtomicBool::new(false)),
            start_count: Arc::new(AtomicU32::new(0)),
            stop_count: Arc::new(AtomicU32::new(0)),
        }
    }

    fn deprovision_called(&self) -> Arc<AtomicBool> {
        self.deprovision_called.clone()
    }

    fn start_count(&self) -> Arc<AtomicU32> {
        self.start_count.clone()
    }

    fn stop_count(&self) -> Arc<AtomicU32> {
        self.stop_count.clone()
    }
}

#[async_trait]
impl Reaction for TestReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "deprovision-test-reaction"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.start_count.fetch_add(1, Ordering::SeqCst);
        let reaction_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_reaction_start",
            instance_id = %instance_id,
            component_id = %reaction_id,
            component_type = "reaction"
        );
        async {
            tracing::info!("TestReaction starting");
        }
        .instrument(span)
        .await;

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.stop_count.fetch_add(1, Ordering::SeqCst);
        let reaction_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_reaction_stop",
            instance_id = %instance_id,
            component_id = %reaction_id,
            component_type = "reaction"
        );
        async {
            tracing::info!("TestReaction stopping");
        }
        .instrument(span)
        .await;

        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        self.deprovision_called.store(true, Ordering::SeqCst);
        self.base.deprovision_common().await?;
        Ok(())
    }
}

// ============================================================================
// Helper
// ============================================================================

async fn collect_events(
    rx: &mut tokio::sync::broadcast::Receiver<ComponentEvent>,
    target_status: ComponentStatus,
    timeout_secs: u64,
) -> Vec<ComponentEvent> {
    let mut events = Vec::new();
    let _ = timeout(Duration::from_secs(timeout_secs), async {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let done = event.status == target_status;
                    events.push(event);
                    if done {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("Lagged {n} events");
                }
                Err(_) => break,
            }
        }
    })
    .await;
    events
}

// ============================================================================
// Source Deprovision Tests
// ============================================================================

/// Removing a source with cleanup=true calls deprovision.
#[tokio::test]
async fn test_source_remove_with_cleanup_calls_deprovision() {
    let source = TestSource::new("deprov-src-1").unwrap();
    let deprov_flag = source.deprovision_called();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    drasi.remove_source("deprov-src-1", true).await.unwrap();

    assert!(
        deprov_flag.load(Ordering::SeqCst),
        "deprovision() should have been called with cleanup=true"
    );

    // Source should no longer be listed
    let sources = drasi.list_sources().await.unwrap();
    assert!(
        !sources.iter().any(|s| s.0 == "deprov-src-1"),
        "Source should be removed"
    );

    drasi.stop().await.unwrap();
}

/// Removing a source with cleanup=false does NOT call deprovision.
#[tokio::test]
async fn test_source_remove_without_cleanup_skips_deprovision() {
    let source = TestSource::new("deprov-src-2").unwrap();
    let deprov_flag = source.deprovision_called();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    drasi.remove_source("deprov-src-2", false).await.unwrap();

    assert!(
        !deprov_flag.load(Ordering::SeqCst),
        "deprovision() should NOT have been called with cleanup=false"
    );

    drasi.stop().await.unwrap();
}

/// Removing a stopped source with cleanup=true still calls deprovision.
#[tokio::test]
async fn test_source_remove_stopped_with_cleanup() {
    let source = TestSource::new("deprov-src-3").unwrap();
    let deprov_flag = source.deprovision_called();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stop first, then remove
    drasi.stop_source("deprov-src-3").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    drasi.remove_source("deprov-src-3", true).await.unwrap();

    assert!(
        deprov_flag.load(Ordering::SeqCst),
        "deprovision() should be called even on a stopped source"
    );

    drasi.stop().await.unwrap();
}

// ============================================================================
// Reaction Deprovision Tests
// ============================================================================

/// Removing a reaction with cleanup=true calls deprovision.
#[tokio::test]
async fn test_reaction_remove_with_cleanup_calls_deprovision() {
    let source = TestSource::new("deprov-rxn-src-1").unwrap();
    let query = Query::cypher("deprov-rxn-q-1")
        .from_source("deprov-rxn-src-1")
        .query("MATCH (n) RETURN n")
        .build();
    let reaction = TestReaction::new("deprov-rxn-1", vec!["deprov-rxn-q-1".to_string()]);
    let deprov_flag = reaction.deprovision_called();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    drasi.remove_reaction("deprov-rxn-1", true).await.unwrap();

    assert!(
        deprov_flag.load(Ordering::SeqCst),
        "deprovision() should have been called with cleanup=true"
    );

    drasi.stop().await.unwrap();
}

/// Removing a reaction with cleanup=false does NOT call deprovision.
#[tokio::test]
async fn test_reaction_remove_without_cleanup_skips_deprovision() {
    let source = TestSource::new("deprov-rxn-src-2").unwrap();
    let query = Query::cypher("deprov-rxn-q-2")
        .from_source("deprov-rxn-src-2")
        .query("MATCH (n) RETURN n")
        .build();
    let reaction = TestReaction::new("deprov-rxn-2", vec!["deprov-rxn-q-2".to_string()]);
    let deprov_flag = reaction.deprovision_called();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    drasi.remove_reaction("deprov-rxn-2", false).await.unwrap();

    assert!(
        !deprov_flag.load(Ordering::SeqCst),
        "deprovision() should NOT have been called with cleanup=false"
    );

    drasi.stop().await.unwrap();
}

// ============================================================================
// Source Reconfigure Tests
// ============================================================================

/// Updating a running source stops it, replaces the instance, and restarts it.
#[tokio::test]
async fn test_source_update_running_stops_and_restarts() {
    let source = TestSource::new("reconfig-src-1").unwrap();
    let start_count = source.start_count();
    let stop_count = source.stop_count();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should be running
    let status = drasi.get_source_status("reconfig-src-1").await.unwrap();
    assert_eq!(status, ComponentStatus::Running);
    assert_eq!(start_count.load(Ordering::SeqCst), 1);

    // Update with a new source instance
    let new_source = TestSource::new("reconfig-src-1").unwrap();
    let new_start_count = new_source.start_count();

    drasi
        .update_source("reconfig-src-1", new_source)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Old instance was stopped
    assert_eq!(stop_count.load(Ordering::SeqCst), 1, "should have stopped old instance");
    // New instance was started
    assert_eq!(
        new_start_count.load(Ordering::SeqCst),
        1,
        "should have started new instance"
    );

    // Should be running again
    let status = drasi.get_source_status("reconfig-src-1").await.unwrap();
    assert_eq!(status, ComponentStatus::Running);

    drasi.stop().await.unwrap();
}

/// Updating a stopped source replaces the instance without restart.
#[tokio::test]
async fn test_source_update_stopped_replaces_without_restart() {
    let source = TestSource::new("reconfig-src-2").unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stop the source
    drasi.stop_source("reconfig-src-2").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let new_source = TestSource::new("reconfig-src-2").unwrap();
    let new_start_count = new_source.start_count();

    drasi
        .update_source("reconfig-src-2", new_source)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // New instance should NOT have been started (was stopped)
    assert_eq!(
        new_start_count.load(Ordering::SeqCst),
        0,
        "should NOT restart a stopped source"
    );

    drasi.stop().await.unwrap();
}

// ============================================================================
// Reaction Reconfigure Tests
// ============================================================================

/// Updating a running reaction stops it, replaces the instance, and restarts it.
#[tokio::test]
async fn test_reaction_update_running_stops_and_restarts() {
    let source = TestSource::new("reconfig-rxn-src-1").unwrap();
    let query = Query::cypher("reconfig-rxn-q-1")
        .from_source("reconfig-rxn-src-1")
        .query("MATCH (n) RETURN n")
        .build();
    let reaction = TestReaction::new("reconfig-rxn-1", vec!["reconfig-rxn-q-1".to_string()]);
    let stop_count = reaction.stop_count();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let new_reaction = TestReaction::new("reconfig-rxn-1", vec!["reconfig-rxn-q-1".to_string()]);
    let new_start_count = new_reaction.start_count();

    drasi
        .update_reaction("reconfig-rxn-1", new_reaction)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(stop_count.load(Ordering::SeqCst), 1, "should have stopped old instance");
    assert_eq!(new_start_count.load(Ordering::SeqCst), 1, "should have started new instance");

    drasi.stop().await.unwrap();
}

// ============================================================================
// Lifecycle Event Streaming Tests
// ============================================================================

/// Verify that Reconfiguring event is emitted during source update.
#[tokio::test]
async fn test_source_update_emits_reconfiguring_event() {
    let source = TestSource::new("reconfig-evt-src").unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Subscribe to events before update
    let (_history, mut event_rx) = drasi
        .subscribe_source_events("reconfig-evt-src")
        .await
        .unwrap();

    let new_source = TestSource::new("reconfig-evt-src").unwrap();

    drasi
        .update_source("reconfig-evt-src", new_source)
        .await
        .unwrap();

    // Collect events — expect Reconfiguring and Running
    let events = collect_events(&mut event_rx, ComponentStatus::Running, 3).await;
    let statuses: Vec<_> = events.iter().map(|e| &e.status).collect();
    println!("Source update events: {statuses:?}");

    assert!(
        events
            .iter()
            .any(|e| e.status == ComponentStatus::Reconfiguring),
        "Expected Reconfiguring event, got: {statuses:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.status == ComponentStatus::Running),
        "Expected Running event after reconfigure, got: {statuses:?}"
    );

    drasi.stop().await.unwrap();
}

/// Verify that Reconfiguring event is emitted during reaction update.
#[tokio::test]
async fn test_reaction_update_emits_reconfiguring_event() {
    let source = TestSource::new("reconfig-evt-rxn-src").unwrap();
    let query = Query::cypher("reconfig-evt-rxn-q")
        .from_source("reconfig-evt-rxn-src")
        .query("MATCH (n) RETURN n")
        .build();
    let reaction =
        TestReaction::new("reconfig-evt-rxn", vec!["reconfig-evt-rxn-q".to_string()]);

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (_history, mut event_rx) = drasi
        .subscribe_reaction_events("reconfig-evt-rxn")
        .await
        .unwrap();

    let new_reaction =
        TestReaction::new("reconfig-evt-rxn", vec!["reconfig-evt-rxn-q".to_string()]);

    drasi
        .update_reaction("reconfig-evt-rxn", new_reaction)
        .await
        .unwrap();

    let events = collect_events(&mut event_rx, ComponentStatus::Running, 3).await;
    let statuses: Vec<_> = events.iter().map(|e| &e.status).collect();
    println!("Reaction update events: {statuses:?}");

    assert!(
        events
            .iter()
            .any(|e| e.status == ComponentStatus::Reconfiguring),
        "Expected Reconfiguring event, got: {statuses:?}"
    );

    drasi.stop().await.unwrap();
}

// ============================================================================
// Log Preservation Tests
// ============================================================================

/// Source log history is preserved across a reconfigure cycle.
#[tokio::test]
async fn test_source_update_preserves_log_history() {
    let source = TestSource::new("reconfig-log-src").unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Capture pre-update log count
    let (pre_logs, _) = drasi
        .subscribe_source_logs("reconfig-log-src")
        .await
        .unwrap();
    let pre_count = pre_logs.len();
    assert!(pre_count > 0, "Expected logs before update");

    // Now update
    let new_source = TestSource::new("reconfig-log-src").unwrap();
    drasi
        .update_source("reconfig-log-src", new_source)
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Post-update logs should include pre-update logs plus new ones
    let (post_logs, _) = drasi
        .subscribe_source_logs("reconfig-log-src")
        .await
        .unwrap();
    assert!(
        post_logs.len() >= pre_count,
        "Log history should be preserved: pre={pre_count}, post={}",
        post_logs.len()
    );

    drasi.stop().await.unwrap();
}

/// Active log subscribers continue receiving logs through a reconfigure.
#[tokio::test]
async fn test_source_update_active_log_stream_continues() {
    let source = TestSource::new("reconfig-stream-src").unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    // Subscribe to logs before start
    let (_initial, mut log_rx) = drasi
        .subscribe_source_logs("reconfig-stream-src")
        .await
        .unwrap();

    drasi.start().await.unwrap();

    // Collect initial startup logs
    let mut pre_logs = Vec::new();
    let _ = timeout(Duration::from_secs(1), async {
        while let Ok(log) = log_rx.recv().await {
            pre_logs.push(log);
            if pre_logs.len() >= 2 {
                break;
            }
        }
    })
    .await;
    assert!(!pre_logs.is_empty(), "Should receive startup logs");

    // Now reconfigure
    let new_source = TestSource::new("reconfig-stream-src").unwrap();
    drasi
        .update_source("reconfig-stream-src", new_source)
        .await
        .unwrap();

    // The same subscriber should receive new logs from the restart
    let mut post_logs = Vec::new();
    let _ = timeout(Duration::from_secs(2), async {
        while let Ok(log) = log_rx.recv().await {
            post_logs.push(log);
            if post_logs.len() >= 2 {
                break;
            }
        }
    })
    .await;

    assert!(
        !post_logs.is_empty(),
        "Active log subscriber should continue receiving logs after reconfigure"
    );

    drasi.stop().await.unwrap();
}

// ============================================================================
// Edge Cases
// ============================================================================

/// Removing a non-existent source returns an error.
#[tokio::test]
async fn test_remove_nonexistent_source_fails() {
    let drasi = DrasiLib::builder().build().await.unwrap();
    drasi.start().await.unwrap();

    let result = drasi.remove_source("does-not-exist", true).await;
    assert!(result.is_err());

    drasi.stop().await.unwrap();
}

/// Updating a non-existent source returns an error.
#[tokio::test]
async fn test_update_nonexistent_source_fails() {
    let drasi = DrasiLib::builder().build().await.unwrap();
    drasi.start().await.unwrap();

    let result = drasi
        .update_source("does-not-exist", TestSource::new("does-not-exist").unwrap())
        .await;
    assert!(result.is_err());

    drasi.stop().await.unwrap();
}

/// Removing a non-existent reaction returns an error.
#[tokio::test]
async fn test_remove_nonexistent_reaction_fails() {
    let drasi = DrasiLib::builder().build().await.unwrap();
    drasi.start().await.unwrap();

    let result = drasi.remove_reaction("does-not-exist", true).await;
    assert!(result.is_err());

    drasi.stop().await.unwrap();
}

/// Updating a non-existent reaction returns an error.
#[tokio::test]
async fn test_update_nonexistent_reaction_fails() {
    let drasi = DrasiLib::builder().build().await.unwrap();
    drasi.start().await.unwrap();

    let result = drasi
        .update_reaction(
            "does-not-exist",
            TestReaction::new("does-not-exist", vec![]),
        )
        .await;
    assert!(result.is_err());

    drasi.stop().await.unwrap();
}

// ============================================================================
// State Store Cleanup Tests
// ============================================================================

/// Removing a source with cleanup=true clears its state store partition.
#[tokio::test]
async fn test_source_remove_with_cleanup_clears_state_store() {
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    // Pre-populate state store with data for this source
    state_store
        .set("cleanup-src", "key1", b"value1".to_vec())
        .await
        .unwrap();
    state_store
        .set("cleanup-src", "key2", b"value2".to_vec())
        .await
        .unwrap();

    let keys = state_store.list_keys("cleanup-src").await.unwrap();
    assert_eq!(keys.len(), 2, "State store should have 2 keys before delete");

    let source = TestSource::new("cleanup-src").unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_state_store_provider(state_store.clone() as Arc<dyn StateStoreProvider>)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    drasi.remove_source("cleanup-src", true).await.unwrap();

    let keys = state_store.list_keys("cleanup-src").await.unwrap();
    assert!(
        keys.is_empty(),
        "State store should be empty after delete with cleanup=true, got {} keys",
        keys.len()
    );

    drasi.stop().await.unwrap();
}

/// Removing a source with cleanup=false preserves its state store partition.
#[tokio::test]
async fn test_source_remove_without_cleanup_preserves_state_store() {
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    // Pre-populate state store with data for this source
    state_store
        .set("no-cleanup-src", "key1", b"value1".to_vec())
        .await
        .unwrap();
    state_store
        .set("no-cleanup-src", "key2", b"value2".to_vec())
        .await
        .unwrap();

    let source = TestSource::new("no-cleanup-src").unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_state_store_provider(state_store.clone() as Arc<dyn StateStoreProvider>)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    drasi.remove_source("no-cleanup-src", false).await.unwrap();

    let keys = state_store.list_keys("no-cleanup-src").await.unwrap();
    assert_eq!(
        keys.len(),
        2,
        "State store should still have 2 keys after delete with cleanup=false"
    );

    drasi.stop().await.unwrap();
}
