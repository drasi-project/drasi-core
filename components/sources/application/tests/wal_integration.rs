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

//! WAL (Write-Ahead Log) integration tests for the Application source.
//!
//! These tests verify durable event persistence, crash recovery, and replay
//! functionality using the redb WAL backend.

#![allow(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ChangeReceiver;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::wal::{CapacityPolicy, WalProvider};
use drasi_lib::Source;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use drasi_wal_redb::RedbWalProvider;
use tempfile::TempDir;

/// Helper: create a DurabilityConfig with the given policy
fn durability_config(
    enabled: bool,
    max_events: u64,
    policy: CapacityPolicy,
) -> drasi_lib::DurabilityConfig {
    drasi_lib::DurabilityConfig {
        enabled,
        max_events,
        capacity_policy: policy,
    }
}

/// Helper: create ApplicationSourceConfig with optional durability
fn app_config(durability: Option<drasi_lib::DurabilityConfig>) -> ApplicationSourceConfig {
    ApplicationSourceConfig {
        properties: HashMap::new(),
        durability,
    }
}

/// Helper: initialize a source with WAL provider
async fn init_source_with_wal(
    source: &ApplicationSource,
    wal: Arc<dyn WalProvider>,
    source_id: &str,
) {
    let (update_tx, _update_rx) = tokio::sync::mpsc::channel(16);
    source
        .initialize(SourceRuntimeContext {
            instance_id: "test-instance".to_string(),
            source_id: source_id.to_string(),
            update_tx,
            state_store: None,
            identity_provider: None,
            wal_provider: Some(wal),
        })
        .await;
}

/// Helper: create subscription settings (no resume)
fn fresh_settings(source_id: &str, query_id: &str) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        query_id: query_id.to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        request_position_handle: true,
        resume_from: None,
        last_sequence: None,
    }
}

/// Helper: create subscription settings with resume position
fn resume_settings(source_id: &str, query_id: &str, resume_seq: u64) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        query_id: query_id.to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        request_position_handle: true,
        resume_from: Some(bytes::Bytes::from(resume_seq.to_be_bytes().to_vec())),
        last_sequence: Some(resume_seq),
    }
}

/// Helper: subscribe with default settings (no resume)
async fn subscribe_fresh(
    source: &ApplicationSource,
    source_id: &str,
) -> Box<dyn ChangeReceiver<drasi_lib::channels::events::SourceEventWrapper>> {
    let resp = source
        .subscribe(fresh_settings(source_id, "test-query"))
        .await
        .unwrap();
    resp.receiver
}

/// Helper: subscribe with resume_from position
async fn subscribe_with_resume(
    source: &ApplicationSource,
    source_id: &str,
    resume_seq: u64,
) -> Box<dyn ChangeReceiver<drasi_lib::channels::events::SourceEventWrapper>> {
    let resp = source
        .subscribe(resume_settings(source_id, "test-query-resume", resume_seq))
        .await
        .unwrap();
    resp.receiver
}

// ============================================================
// Unit-level WAL tests
// ============================================================

#[tokio::test]
#[ignore]
async fn test_wal_disabled_no_persistence() {
    let config = app_config(None);
    let (source, _handle) = ApplicationSource::new("disabled-src", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "disabled-src").await;

    source.start().await.unwrap();
    assert!(!source.supports_replay());

    // WAL should not be registered
    let count = wal.event_count("disabled-src").await;
    assert!(
        count.is_err(),
        "WAL should not be registered for disabled source"
    );

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_wal_enabled_events_persisted() {
    let config = app_config(Some(durability_config(
        true,
        10_000,
        CapacityPolicy::RejectIncoming,
    )));
    let (source, handle) = ApplicationSource::new("persist-src", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "persist-src").await;

    source.start().await.unwrap();
    assert!(source.supports_replay());

    let mut rx = subscribe_fresh(&source, "persist-src").await;

    // Send events
    let props = PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .build();
    handle
        .send_node_insert("node-1", vec!["Person"], props)
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(event.sequence.is_some());
    assert_eq!(event.sequence.unwrap(), 1);
    assert!(event.source_position.is_some());

    // Verify WAL has the event
    let count = wal.event_count("persist-src").await.unwrap();
    assert_eq!(count, 1);

    // Send more events
    let props2 = PropertyMapBuilder::new().with_string("name", "Bob").build();
    handle
        .send_node_insert("node-2", vec!["Person"], props2)
        .await
        .unwrap();
    let event2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event2.sequence.unwrap(), 2);

    let count = wal.event_count("persist-src").await.unwrap();
    assert_eq!(count, 2);

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_capacity_exhaustion_reject_incoming() {
    // redb WAL requires max_events >= 16
    let config = app_config(Some(durability_config(
        true,
        16,
        CapacityPolicy::RejectIncoming,
    )));
    let (source, handle) = ApplicationSource::new("reject-src", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "reject-src").await;

    source.start().await.unwrap();

    let mut rx = subscribe_fresh(&source, "reject-src").await;

    // Fill WAL to capacity (max_events=16)
    for i in 1..=16 {
        let props = PropertyMapBuilder::new().with_integer("x", i).build();
        handle
            .send_node_insert(format!("n{i}"), vec!["T"], props)
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap();
    }

    // 17th event — WAL capacity exhausted, send returns error to caller
    let props = PropertyMapBuilder::new().with_integer("x", 17).build();
    let result = handle.send_node_insert("n17", vec!["T"], props).await;
    assert!(result.is_err(), "Expected capacity exhaustion error");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("capacity exhausted"),
        "Error should mention capacity exhaustion"
    );

    // WAL should still have only 16 events
    let count = wal.event_count("reject-src").await.unwrap();
    assert_eq!(count, 16);

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_deprovision_removes_wal() {
    let config = app_config(Some(durability_config(
        true,
        10_000,
        CapacityPolicy::RejectIncoming,
    )));
    let (source, handle) = ApplicationSource::new("deprov-src", config).unwrap();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    init_source_with_wal(&source, wal.clone(), "deprov-src").await;

    source.start().await.unwrap();
    let mut rx = subscribe_fresh(&source, "deprov-src").await;

    let props = PropertyMapBuilder::new().with_string("x", "1").build();
    handle
        .send_node_insert("n1", vec!["T"], props)
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap();

    let count = wal.event_count("deprov-src").await.unwrap();
    assert!(count > 0);

    source.stop().await.unwrap();
    source.deprovision().await.unwrap();

    let count = wal.event_count("deprov-src").await;
    assert!(count.is_err(), "WAL should be deleted after deprovision");
}

// ============================================================
// Integration tests: crash recovery and replay
// ============================================================

#[tokio::test]
#[ignore]
async fn test_crash_recovery_resumes_sequence() {
    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));

    // First "lifecycle" — write some events
    {
        let config = app_config(Some(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        )));
        let (source, handle) = ApplicationSource::new("crash-src", config).unwrap();
        init_source_with_wal(&source, wal.clone(), "crash-src").await;
        source.start().await.unwrap();

        let mut rx = subscribe_fresh(&source, "crash-src").await;

        for i in 1..=5 {
            let props = PropertyMapBuilder::new().with_integer("i", i).build();
            handle
                .send_node_insert(format!("n{i}"), vec!["T"], props)
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap();
        }

        let head = wal.head_sequence("crash-src").await.unwrap();
        assert_eq!(head, 5);

        source.stop().await.unwrap();
    }

    // Second "lifecycle" — should resume from sequence 5
    {
        let config = app_config(Some(durability_config(
            true,
            10_000,
            CapacityPolicy::RejectIncoming,
        )));
        let (source, handle) = ApplicationSource::new("crash-src", config).unwrap();
        init_source_with_wal(&source, wal.clone(), "crash-src").await;
        source.start().await.unwrap();

        let mut rx = subscribe_fresh(&source, "crash-src").await;

        // New events should start at seq 6
        let props = PropertyMapBuilder::new().with_integer("i", 6).build();
        handle
            .send_node_insert("n6", vec!["T"], props)
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.sequence.unwrap(), 6);

        source.stop().await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn test_replay_via_subscribe() {
    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));

    let config = app_config(Some(durability_config(
        true,
        10_000,
        CapacityPolicy::RejectIncoming,
    )));
    let (source, handle) = ApplicationSource::new("replay-src", config).unwrap();
    init_source_with_wal(&source, wal.clone(), "replay-src").await;
    source.start().await.unwrap();

    let mut rx1 = subscribe_fresh(&source, "replay-src").await;

    // Send 5 events
    for i in 1..=5 {
        let props = PropertyMapBuilder::new().with_integer("i", i).build();
        handle
            .send_node_insert(format!("n{i}"), vec!["T"], props)
            .await
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), rx1.recv())
            .await
            .unwrap();
    }

    // Subscribe with resume_from = seq 2 → should replay events 3, 4, 5
    let mut rx2 = subscribe_with_resume(&source, "replay-src", 2).await;

    for expected_seq in 3..=5 {
        let event = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event.sequence.unwrap(),
            expected_seq,
            "Expected replay seq {expected_seq}"
        );
    }

    // Send a new event — both subscribers should get it
    let props = PropertyMapBuilder::new().with_integer("i", 6).build();
    handle
        .send_node_insert("n6", vec!["T"], props)
        .await
        .unwrap();

    let event1 = tokio::time::timeout(Duration::from_secs(2), rx1.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event1.sequence.unwrap(), 6);

    let event2 = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event2.sequence.unwrap(), 6);

    source.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_concurrent_writes_monotonic_sequences() {
    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));

    let config = app_config(Some(durability_config(
        true,
        10_000,
        CapacityPolicy::RejectIncoming,
    )));
    let (source, handle) = ApplicationSource::new("conc-src", config).unwrap();
    init_source_with_wal(&source, wal.clone(), "conc-src").await;
    source.start().await.unwrap();

    let mut rx = subscribe_fresh(&source, "conc-src").await;

    let total_events: usize = 100;
    let tasks_count = 4;
    let events_per_task = total_events / tasks_count;

    let mut tasks = vec![];
    for task_id in 0..tasks_count {
        let h = handle.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..events_per_task {
                let props = PropertyMapBuilder::new()
                    .with_integer("task", task_id as i64)
                    .with_integer("i", i as i64)
                    .build();
                h.send_node_insert(format!("t{task_id}-n{i}"), vec!["T"], props)
                    .await
                    .unwrap();
            }
        }));
    }

    for t in tasks {
        t.await.unwrap();
    }

    // Collect all received events
    let mut sequences = vec![];
    for _ in 0..total_events {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        sequences.push(event.sequence.unwrap());
    }

    // Verify all sequences are unique and cover 1..=total_events
    sequences.sort();
    let expected: Vec<u64> = (1..=total_events as u64).collect();
    assert_eq!(
        sequences, expected,
        "Sequences should be 1..={total_events} with no gaps"
    );

    let count = wal.event_count("conc-src").await.unwrap();
    assert_eq!(count, total_events as u64);

    source.stop().await.unwrap();
}

/// End-to-end test: validates the full resume-from-position lifecycle.
///
/// Flow:
/// 1. Start source with WAL, subscribe query, receive events
/// 2. Subscriber advances its position handle (simulating query checkpoint)
/// 3. Stop source (simulating crash)
/// 4. Restart source (same WAL path → persisted events survive)
/// 5. New subscriber connects with `resume_from` = last confirmed position
/// 6. Verify it receives ONLY events after the checkpoint position
/// 7. Verify new live events also arrive correctly
#[tokio::test]
#[ignore]
async fn test_resume_from_position_end_to_end() {
    let tmp = TempDir::new().unwrap();

    // Phase 1: Start source and publish events
    let wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(tmp.path()));

    let config = app_config(Some(durability_config(
        true,
        10_000,
        CapacityPolicy::RejectIncoming,
    )));
    let (source, handle) = ApplicationSource::new("e2e-src", config).unwrap();
    init_source_with_wal(&source, wal.clone(), "e2e-src").await;
    source.start().await.unwrap();

    // Subscribe with position handle (simulates a real query subscriber)
    let settings = SourceSubscriptionSettings {
        source_id: "e2e-src".to_string(),
        query_id: "e2e-query".to_string(),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        request_position_handle: true,
        resume_from: None,
        last_sequence: None,
    };
    let resp = source.subscribe(settings).await.unwrap();
    let mut rx = resp.receiver;
    let position_handle = resp.position_handle.expect("should have position handle");

    // Publish 5 events
    for i in 1..=5 {
        let props = PropertyMapBuilder::new().with_integer("val", i).build();
        handle
            .send_node_insert(format!("n{i}"), vec!["Item"], props)
            .await
            .unwrap();
    }

    // Receive all 5 and advance position handle to simulate checkpoint
    let mut last_confirmed_seq = 0u64;
    for _ in 0..5 {
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for event")
            .unwrap();
        let seq = event.sequence.expect("event should have WAL sequence");
        // Simulate query confirming progress by advancing position handle
        position_handle.store(seq, std::sync::atomic::Ordering::Release);
        last_confirmed_seq = seq;
    }
    assert_eq!(last_confirmed_seq, 5, "Should have confirmed through seq 5");

    // Publish 3 more events (these are "unconfirmed" by the query — i.e. after checkpoint)
    for i in 6..=8 {
        let props = PropertyMapBuilder::new().with_integer("val", i).build();
        handle
            .send_node_insert(format!("n{i}"), vec!["Item"], props)
            .await
            .unwrap();
    }

    // Receive them (so they flow through)
    for _ in 0..3 {
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .unwrap();
    }

    // Phase 2: Stop source (simulating crash after partial checkpoint)
    source.stop().await.unwrap();
    drop(rx);
    drop(handle);
    drop(source);

    // Verify WAL still has events persisted
    let wal_count = wal.event_count("e2e-src").await.unwrap();
    assert!(
        wal_count >= 3,
        "WAL should have at least the unconfirmed events, got {wal_count}"
    );

    // Phase 3: Restart source from the same WAL
    let config2 = app_config(Some(durability_config(
        true,
        10_000,
        CapacityPolicy::RejectIncoming,
    )));
    let (source2, handle2) = ApplicationSource::new("e2e-src", config2).unwrap();
    init_source_with_wal(&source2, wal.clone(), "e2e-src").await;
    source2.start().await.unwrap();

    // Phase 4: Subscribe with resume_from = last_confirmed_seq (checkpoint position)
    // This simulates a query reconnecting after crash and saying "I last confirmed seq 5"
    let mut rx2 = subscribe_with_resume(&source2, "e2e-src", last_confirmed_seq).await;

    // Should receive replay of events 6, 7, 8 (those after the checkpoint)
    let mut replayed_seqs = vec![];
    for _ in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
            .await
            .expect("timed out waiting for replay event")
            .unwrap();
        replayed_seqs.push(event.sequence.expect("replay event should have sequence"));
    }
    assert_eq!(
        replayed_seqs,
        vec![6, 7, 8],
        "Should replay events 6, 7, 8 after checkpoint"
    );

    // Phase 5: New live events should also flow through
    let props = PropertyMapBuilder::new().with_integer("val", 9).build();
    handle2
        .send_node_insert("n9", vec!["Item"], props)
        .await
        .unwrap();

    let live_event = tokio::time::timeout(Duration::from_secs(2), rx2.recv())
        .await
        .expect("timed out waiting for live event")
        .unwrap();
    assert_eq!(
        live_event
            .sequence
            .expect("live event should have sequence"),
        9,
        "Live event after restart should continue sequence"
    );

    // Verify source_position is set correctly (big-endian u64 bytes)
    let pos = live_event
        .source_position
        .as_ref()
        .expect("should have source_position");
    let pos_seq = u64::from_be_bytes(pos[..8].try_into().unwrap());
    assert_eq!(
        pos_seq, 9,
        "source_position should encode the sequence number"
    );

    source2.stop().await.unwrap();
}
