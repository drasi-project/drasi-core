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

//! Comprehensive tests for `RedbWalProvider`.
//!
//! Tests verify against the `WalProvider` trait (via `Arc<dyn WalProvider>`)
//! so any future implementation can reuse the same test harness.

#![allow(clippy::unwrap_used)]

use super::RedbWalProvider;
use drasi_core::interface::FutureElementRef;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::{CapacityPolicy, WalError, WalProvider, WriteAheadLogConfig};
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const TEST_SRC: &str = "test-src";

fn default_config() -> WriteAheadLogConfig {
    WriteAheadLogConfig::default()
}

fn small_config(max_events: u64, policy: CapacityPolicy) -> WriteAheadLogConfig {
    WriteAheadLogConfig {
        max_events,
        capacity_policy: policy,
    }
}

/// Create a provider wrapped as `Arc<dyn WalProvider>` so tests exercise the
/// trait contract rather than the concrete type.
fn new_provider(root: &std::path::Path) -> Arc<dyn WalProvider> {
    Arc::new(RedbWalProvider::new(root))
}

async fn register_default(provider: &Arc<dyn WalProvider>, source_id: &str) {
    provider
        .register(source_id, default_config())
        .await
        .unwrap();
}

fn make_test_insert(id: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: Arc::from(vec![Arc::from("TestNode")]),
                effective_from: 1000,
            },
            properties: {
                let mut props = ElementPropertyMap::new();
                props.insert(
                    "name",
                    ElementValue::String(Arc::from(format!("node_{id}"))),
                );
                props.insert("value", ElementValue::Integer(42));
                props
            },
        },
    }
}

fn make_test_update(id: &str) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: Arc::from(vec![Arc::from("TestNode")]),
                effective_from: 2000,
            },
            properties: {
                let mut props = ElementPropertyMap::new();
                props.insert(
                    "name",
                    ElementValue::String(Arc::from(format!("updated_{id}"))),
                );
                props
            },
        },
    }
}

fn make_test_delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", id),
            labels: Arc::from(vec![Arc::from("TestNode")]),
            effective_from: 3000,
        },
    }
}

fn make_test_future(id: &str) -> SourceChange {
    SourceChange::Future {
        future_ref: FutureElementRef {
            element_ref: ElementReference::new("test_source", id),
            original_time: 0,
            due_time: 100,
            group_signature: 0,
        },
    }
}

// ---------------------------------------------------------------------------
// Basic operations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_append_and_read() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    let seq1 = provider
        .append(TEST_SRC, &make_test_insert("n1"))
        .await
        .unwrap();
    let seq2 = provider
        .append(TEST_SRC, &make_test_insert("n2"))
        .await
        .unwrap();
    let seq3 = provider
        .append(TEST_SRC, &make_test_update("n1"))
        .await
        .unwrap();

    let events = provider.read_from(TEST_SRC, seq1).await.unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].0, seq1);
    assert_eq!(events[1].0, seq2);
    assert_eq!(events[2].0, seq3);
    assert_eq!(events[0].1, make_test_insert("n1"));
    assert_eq!(events[1].1, make_test_insert("n2"));
    assert_eq!(events[2].1, make_test_update("n1"));
}

#[tokio::test]
async fn test_append_returns_monotonic_sequences() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    let mut prev = 0;
    for i in 0..20 {
        let seq = provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
        assert!(seq > prev, "seq {seq} should be > prev {prev}");
        prev = seq;
    }
}

#[tokio::test]
async fn test_read_from_specific_sequence() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    let mut sequences = Vec::new();
    for i in 0..10 {
        let seq = provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
        sequences.push(seq);
    }

    let events = provider.read_from(TEST_SRC, sequences[4]).await.unwrap();
    assert_eq!(events.len(), 6);
    assert_eq!(events[0].0, sequences[4]);
    assert_eq!(events[5].0, sequences[9]);
}

#[tokio::test]
async fn test_read_from_pruned_position() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    for i in 0..5 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }
    provider.prune_up_to(TEST_SRC, 3).await.unwrap();

    let result = provider.read_from(TEST_SRC, 1).await;
    match result {
        Err(WalError::PositionUnavailable {
            source_id,
            requested,
            oldest_available,
        }) => {
            assert_eq!(source_id, TEST_SRC);
            assert_eq!(requested, 1);
            assert_eq!(oldest_available, Some(4));
        }
        other => panic!("Expected PositionUnavailable, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_read_empty_wal() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    let events = provider.read_from(TEST_SRC, 1).await.unwrap();
    assert!(events.is_empty());
}

#[tokio::test]
async fn test_head_sequence() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 0);

    provider
        .append(TEST_SRC, &make_test_insert("n1"))
        .await
        .unwrap();
    assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 1);

    provider
        .append(TEST_SRC, &make_test_insert("n2"))
        .await
        .unwrap();
    assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 2);
}

#[tokio::test]
async fn test_oldest_sequence() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), None);

    provider
        .append(TEST_SRC, &make_test_insert("n1"))
        .await
        .unwrap();
    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), Some(1));

    provider
        .append(TEST_SRC, &make_test_insert("n2"))
        .await
        .unwrap();
    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), Some(1));
}

#[tokio::test]
async fn test_event_count() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 0);
    for i in 0..5 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }
    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 5);
}

// ---------------------------------------------------------------------------
// Pruning
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_prune_up_to() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    for i in 0..10 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }

    let pruned = provider.prune_up_to(TEST_SRC, 5).await.unwrap();
    assert_eq!(pruned, 5);
    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 5);

    let events = provider.read_from(TEST_SRC, 6).await.unwrap();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0].0, 6);
}

#[tokio::test]
async fn test_prune_all() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    for i in 0..5 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }
    let pruned = provider.prune_up_to(TEST_SRC, u64::MAX).await.unwrap();
    assert_eq!(pruned, 5);
    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 0);
    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), None);
}

#[tokio::test]
async fn test_prune_returns_count() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    for i in 0..8 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }

    assert_eq!(provider.prune_up_to(TEST_SRC, 3).await.unwrap(), 3);
    assert_eq!(provider.prune_up_to(TEST_SRC, 6).await.unwrap(), 3);
    assert_eq!(provider.prune_up_to(TEST_SRC, 6).await.unwrap(), 0);
}

#[tokio::test]
async fn test_oldest_sequence_after_prune() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    for i in 0..5 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }

    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), Some(1));

    provider.prune_up_to(TEST_SRC, 3).await.unwrap();
    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), Some(4));

    provider.prune_up_to(TEST_SRC, 5).await.unwrap();
    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), None);
}

// ---------------------------------------------------------------------------
// Persistence across provider restarts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_persistence_across_restarts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    // Write events with first provider instance
    {
        let provider = new_provider(&root);
        register_default(&provider, TEST_SRC).await;
        provider
            .append(TEST_SRC, &make_test_insert("n1"))
            .await
            .unwrap();
        provider
            .append(TEST_SRC, &make_test_insert("n2"))
            .await
            .unwrap();
        provider
            .append(TEST_SRC, &make_test_insert("n3"))
            .await
            .unwrap();
    }

    // New provider, same root — should see the same events and counter
    {
        let provider = new_provider(&root);
        register_default(&provider, TEST_SRC).await;

        assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 3);
        assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 3);

        let events = provider.read_from(TEST_SRC, 1).await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].1, make_test_insert("n1"));
        assert_eq!(events[2].1, make_test_insert("n3"));
    }
}

#[tokio::test]
async fn test_counter_resumes_after_restart() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    {
        let provider = new_provider(&root);
        register_default(&provider, TEST_SRC).await;
        provider
            .append(TEST_SRC, &make_test_insert("n1"))
            .await
            .unwrap();
        provider
            .append(TEST_SRC, &make_test_insert("n2"))
            .await
            .unwrap();
        assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 2);
    }

    {
        let provider = new_provider(&root);
        register_default(&provider, TEST_SRC).await;
        assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 2);
        let seq = provider
            .append(TEST_SRC, &make_test_insert("n3"))
            .await
            .unwrap();
        assert_eq!(seq, 3);
        assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 3);
    }
}

// ---------------------------------------------------------------------------
// Capacity management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reject_incoming_on_capacity() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    provider
        .register(TEST_SRC, small_config(16, CapacityPolicy::RejectIncoming))
        .await
        .unwrap();

    for i in 0..16 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }

    let result = provider
        .append(TEST_SRC, &make_test_insert("overflow"))
        .await;
    match result {
        Err(WalError::CapacityExhausted(sid)) => assert_eq!(sid, TEST_SRC),
        other => panic!("Expected CapacityExhausted, got: {other:?}"),
    }

    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 16);
}

#[tokio::test]
async fn test_overwrite_oldest_on_capacity() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    provider
        .register(TEST_SRC, small_config(16, CapacityPolicy::OverwriteOldest))
        .await
        .unwrap();

    for i in 0..16 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }

    let seq17 = provider
        .append(TEST_SRC, &make_test_insert("n16"))
        .await
        .unwrap();
    assert_eq!(seq17, 17);

    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 16);
    assert_eq!(provider.oldest_sequence(TEST_SRC).await.unwrap(), Some(2));
}

#[tokio::test]
async fn test_overwrite_preserves_newest() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    provider
        .register(TEST_SRC, small_config(16, CapacityPolicy::OverwriteOldest))
        .await
        .unwrap();

    // Fill to capacity: sequences 1..=16
    for i in 0..16 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }
    // Two more should evict seqs 1 and 2
    provider
        .append(TEST_SRC, &make_test_insert("x"))
        .await
        .unwrap();
    provider
        .append(TEST_SRC, &make_test_insert("y"))
        .await
        .unwrap();

    let events = provider.read_from(TEST_SRC, 3).await.unwrap();
    assert_eq!(events.len(), 16);
    assert_eq!(events[0].0, 3);
    assert_eq!(events[15].0, 18);
}

#[tokio::test]
async fn test_overwrite_oldest_converges_after_capacity_reduction() {
    // Simulate: original capacity 32, 32 events written. Then reopen with
    // smaller capacity (16). Next append must evict down to exactly max_events.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    {
        let provider = new_provider(&root);
        provider
            .register(TEST_SRC, small_config(32, CapacityPolicy::OverwriteOldest))
            .await
            .unwrap();
        for i in 0..32 {
            provider
                .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
                .await
                .unwrap();
        }
        assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 32);
    }

    // Reopen with smaller max_events
    {
        let provider = new_provider(&root);
        provider
            .register(TEST_SRC, small_config(16, CapacityPolicy::OverwriteOldest))
            .await
            .unwrap();

        // Single append should trigger eviction loop until under new max
        provider
            .append(TEST_SRC, &make_test_insert("new"))
            .await
            .unwrap();

        assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 16);
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rejects_invalid_config_below_min_max_events() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    let result = provider
        .register(TEST_SRC, small_config(0, CapacityPolicy::RejectIncoming))
        .await;
    assert!(matches!(result, Err(WalError::InvalidConfig(_))));

    let result = provider
        .register(TEST_SRC, small_config(15, CapacityPolicy::RejectIncoming))
        .await;
    assert!(matches!(result, Err(WalError::InvalidConfig(_))));

    // Exactly at MIN_MAX_EVENTS should work
    provider
        .register(TEST_SRC, small_config(16, CapacityPolicy::RejectIncoming))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_reject_future_variant() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    let result = provider.append(TEST_SRC, &make_test_future("f1")).await;
    assert!(matches!(result, Err(WalError::InvalidEvent(_))));
    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 0);
    assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 0);
}

#[tokio::test]
async fn test_all_non_future_variants() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    let insert = make_test_insert("n1");
    let update = make_test_update("n1");
    let delete = make_test_delete("n1");

    provider.append(TEST_SRC, &insert).await.unwrap();
    provider.append(TEST_SRC, &update).await.unwrap();
    provider.append(TEST_SRC, &delete).await.unwrap();

    let events = provider.read_from(TEST_SRC, 1).await.unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].1, insert);
    assert_eq!(events[1].1, update);
    assert_eq!(events[2].1, delete);
}

#[tokio::test]
async fn test_source_not_registered_errors() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    let result = provider.append("unknown", &make_test_insert("n1")).await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));

    let result = provider.read_from("unknown", 1).await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));

    let result = provider.prune_up_to("unknown", 5).await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));

    let result = provider.head_sequence("unknown").await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));

    let result = provider.oldest_sequence("unknown").await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));

    let result = provider.event_count("unknown").await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));
}

// ---------------------------------------------------------------------------
// Register idempotency & conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_register_idempotent_same_config() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    provider.register(TEST_SRC, default_config()).await.unwrap();
    provider.register(TEST_SRC, default_config()).await.unwrap();

    // Should still work normally
    provider
        .append(TEST_SRC, &make_test_insert("n1"))
        .await
        .unwrap();
    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 1);
}

#[tokio::test]
async fn test_register_conflict_different_config() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    provider
        .register(TEST_SRC, small_config(100, CapacityPolicy::RejectIncoming))
        .await
        .unwrap();

    let result = provider
        .register(TEST_SRC, small_config(200, CapacityPolicy::RejectIncoming))
        .await;
    assert!(matches!(result, Err(WalError::SourceAlreadyRegistered(_))));

    let result = provider
        .register(TEST_SRC, small_config(100, CapacityPolicy::OverwriteOldest))
        .await;
    assert!(matches!(result, Err(WalError::SourceAlreadyRegistered(_))));
}

// ---------------------------------------------------------------------------
// Multi-source isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multiple_sources_isolated() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    provider.register("src-a", default_config()).await.unwrap();
    provider.register("src-b", default_config()).await.unwrap();

    // Interleaved writes to different sources
    let a1 = provider
        .append("src-a", &make_test_insert("a1"))
        .await
        .unwrap();
    let b1 = provider
        .append("src-b", &make_test_insert("b1"))
        .await
        .unwrap();
    let a2 = provider
        .append("src-a", &make_test_insert("a2"))
        .await
        .unwrap();
    let b2 = provider
        .append("src-b", &make_test_insert("b2"))
        .await
        .unwrap();

    // Each source has its own counter starting at 1
    assert_eq!(a1, 1);
    assert_eq!(b1, 1);
    assert_eq!(a2, 2);
    assert_eq!(b2, 2);

    let a_events = provider.read_from("src-a", 1).await.unwrap();
    let b_events = provider.read_from("src-b", 1).await.unwrap();

    assert_eq!(a_events.len(), 2);
    assert_eq!(b_events.len(), 2);
    assert_eq!(a_events[0].1, make_test_insert("a1"));
    assert_eq!(a_events[1].1, make_test_insert("a2"));
    assert_eq!(b_events[0].1, make_test_insert("b1"));
    assert_eq!(b_events[1].1, make_test_insert("b2"));

    // Pruning one source doesn't affect the other
    provider.prune_up_to("src-a", 100).await.unwrap();
    assert_eq!(provider.event_count("src-a").await.unwrap(), 0);
    assert_eq!(provider.event_count("src-b").await.unwrap(), 2);
}

// ---------------------------------------------------------------------------
// delete_wal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_wal_clears_state_and_file() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    provider
        .append(TEST_SRC, &make_test_insert("n1"))
        .await
        .unwrap();

    let wal_path = tmp.path().join(format!("{TEST_SRC}.redb"));
    assert!(wal_path.exists());

    provider.delete_wal(TEST_SRC).await.unwrap();

    assert!(!wal_path.exists());
    let result = provider.append(TEST_SRC, &make_test_insert("n2")).await;
    assert!(matches!(result, Err(WalError::SourceNotRegistered(_))));
}

#[tokio::test]
async fn test_delete_wal_nonexistent_source() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    // Should succeed (best-effort cleanup)
    provider.delete_wal("never-registered").await.unwrap();
}

// ---------------------------------------------------------------------------
// DTO + bincode round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dto_roundtrip() {
    // Exercises a complex SourceChange with relation + many value types.
    let insert = SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("src", "rel-1"),
                labels: Arc::from(vec![Arc::from("KNOWS"), Arc::from("FRIEND")]),
                effective_from: 999_999,
            },
            in_node: ElementReference::new("src", "person-1"),
            out_node: ElementReference::new("src", "person-2"),
            properties: {
                let mut props = ElementPropertyMap::new();
                props.insert("since", ElementValue::Integer(2020));
                props.insert(
                    "weight",
                    ElementValue::Float(ordered_float::OrderedFloat(0.95)),
                );
                props.insert("active", ElementValue::Bool(true));
                props.insert(
                    "tags",
                    ElementValue::List(vec![
                        ElementValue::String(Arc::from("close")),
                        ElementValue::String(Arc::from("work")),
                    ]),
                );
                props.insert("empty", ElementValue::Null);
                props
            },
        },
    };

    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    register_default(&provider, TEST_SRC).await;

    provider.append(TEST_SRC, &insert).await.unwrap();
    let events = provider.read_from(TEST_SRC, 1).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, insert);
}

// ---------------------------------------------------------------------------
// Concurrent appends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_appends() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    provider
        .register(TEST_SRC, small_config(1000, CapacityPolicy::RejectIncoming))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..10 {
        let p = provider.clone();
        handles.push(tokio::spawn(async move {
            let mut seqs = Vec::new();
            for j in 0..10 {
                let seq = p
                    .append(TEST_SRC, &make_test_insert(&format!("t{i}_n{j}")))
                    .await
                    .unwrap();
                seqs.push(seq);
            }
            seqs
        }));
    }

    let mut all = Vec::new();
    for h in handles {
        all.extend(h.await.unwrap());
    }
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 100);
    assert_eq!(provider.event_count(TEST_SRC).await.unwrap(), 100);
}

// ---------------------------------------------------------------------------
// source_id validation (rejects path traversal, empty, non-allowed chars)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_register_rejects_invalid_source_ids() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    let bad_ids = [
        "",
        "..",
        ".",
        ".hidden",
        "../escape",
        "a/b",
        "a\\b",
        "with space",
        "emoji😀",
        "has:colon",
        "a\0b",
    ];

    for bad in bad_ids {
        let err = provider.register(bad, default_config()).await.unwrap_err();
        assert!(
            matches!(err, WalError::InvalidSourceId(ref got, _) if got == bad),
            "expected InvalidSourceId for {bad:?}, got {err:?}"
        );
    }

    // The provider's root dir must not contain any stray files created by the
    // rejected ids.
    let mut entries = tokio::fs::read_dir(tmp.path()).await.unwrap();
    assert!(
        entries.next_entry().await.unwrap().is_none(),
        "no files should be created for rejected source_ids"
    );
}

#[tokio::test]
async fn test_register_accepts_valid_source_ids() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    for good in ["src", "SRC_1", "my-source-42", "A", "0"] {
        provider.register(good, default_config()).await.unwrap();
    }
}

#[tokio::test]
async fn test_delete_wal_rejects_invalid_source_id() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    let err = provider.delete_wal("../escape").await.unwrap_err();
    assert!(matches!(err, WalError::InvalidSourceId(_, _)));
}

// ---------------------------------------------------------------------------
// Concurrent registration for the same source_id (TOCTOU guard)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_register_same_source_is_safe() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    let mut handles = Vec::new();
    for _ in 0..16 {
        let p = provider.clone();
        handles.push(tokio::spawn(async move {
            p.register(TEST_SRC, default_config()).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }

    // Exactly one state should exist; subsequent ops succeed against it.
    let seq = provider
        .append(TEST_SRC, &make_test_insert("n1"))
        .await
        .unwrap();
    assert_eq!(seq, 1);
}

#[tokio::test]
async fn test_concurrent_register_conflicting_configs_errors_once() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());

    // One caller registers with the default config; a racing caller tries
    // a different config. The provider must accept exactly one registration
    // and reject the conflicting one with SourceAlreadyRegistered.
    let p1 = provider.clone();
    let p2 = provider.clone();
    let a = tokio::spawn(async move { p1.register(TEST_SRC, default_config()).await });
    let b = tokio::spawn(async move {
        p2.register(TEST_SRC, small_config(32, CapacityPolicy::OverwriteOldest))
            .await
    });

    let ra = a.await.unwrap();
    let rb = b.await.unwrap();

    let (oks, errs): (Vec<_>, Vec<_>) = [ra, rb].into_iter().partition(Result::is_ok);
    assert_eq!(oks.len(), 1, "exactly one register should succeed");
    assert_eq!(errs.len(), 1, "exactly one register should conflict");
    assert!(matches!(
        errs.into_iter().next().unwrap().unwrap_err(),
        WalError::SourceAlreadyRegistered(_)
    ));
}

// ---------------------------------------------------------------------------
// Sequence contiguity on rejected append + corrupt counter recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rejected_append_does_not_consume_sequence() {
    let tmp = TempDir::new().unwrap();
    let provider = new_provider(tmp.path());
    provider
        .register(TEST_SRC, small_config(16, CapacityPolicy::RejectIncoming))
        .await
        .unwrap();

    // Fill to capacity.
    for i in 0..16 {
        provider
            .append(TEST_SRC, &make_test_insert(&format!("n{i}")))
            .await
            .unwrap();
    }
    assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 16);

    // A rejected append must not advance the counter.
    let err = provider
        .append(TEST_SRC, &make_test_insert("rejected"))
        .await
        .unwrap_err();
    assert!(matches!(err, WalError::CapacityExhausted(_)));
    assert_eq!(
        provider.head_sequence(TEST_SRC).await.unwrap(),
        16,
        "rejected append must not consume a sequence number"
    );

    // After pruning, the next accepted append picks up at 17 — contiguous
    // with the last successful append rather than skipping past a burned
    // sequence from the rejected call.
    provider.prune_up_to(TEST_SRC, 8).await.unwrap();
    let seq = provider
        .append(TEST_SRC, &make_test_insert("accepted"))
        .await
        .unwrap();
    assert_eq!(seq, 17);
    assert_eq!(provider.head_sequence(TEST_SRC).await.unwrap(), 17);
}

#[tokio::test]
async fn test_corrupt_counter_errors_on_open() {
    let tmp = TempDir::new().unwrap();

    // Create a WAL with one event, then let the provider go out of scope so
    // the redb file is closed and can be reopened for tampering.
    {
        let provider = new_provider(tmp.path());
        provider.register(TEST_SRC, default_config()).await.unwrap();
        provider
            .append(TEST_SRC, &make_test_insert("n1"))
            .await
            .unwrap();
    }

    // Overwrite the persisted counter with a non-8-byte value.
    let wal_path = tmp.path().join(format!("{TEST_SRC}.redb"));
    {
        let db = redb::Database::create(&wal_path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let metadata: redb::TableDefinition<&str, &[u8]> =
                redb::TableDefinition::new("metadata");
            let mut table = write_txn.open_table(metadata).unwrap();
            table.insert("counter", [1u8, 2, 3, 4].as_slice()).unwrap();
        }
        write_txn.commit().unwrap();
    }

    // Reopening must fail loudly rather than silently resetting the counter
    // to 0, which would cause the next append to clobber existing events.
    let provider = new_provider(tmp.path());
    let err = provider
        .register(TEST_SRC, default_config())
        .await
        .unwrap_err();
    assert!(
        matches!(err, WalError::StorageError(ref msg) if msg.contains("corrupt")),
        "expected StorageError about corrupt counter, got {err:?}"
    );
}
