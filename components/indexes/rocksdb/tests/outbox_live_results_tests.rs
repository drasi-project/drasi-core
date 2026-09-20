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

#![allow(clippy::unwrap_used)]

//! Integration tests for RocksDB OutboxWriter and LiveResultsWriter implementations.
//!
//! These tests verify correct persistence, crash recovery (re-open), and boundary
//! conditions for the outbox and live results column families.

use std::sync::Arc;

use drasi_core::interface::{
    IndexBackendPlugin, LiveResultsWriter, OutboxWriter, RowMutation, SessionControl, SessionGuard,
};
use drasi_index_rocksdb::{
    open_unified_db, RocksDbIndexProvider, RocksDbLiveResultsWriter, RocksDbMemoryBudget,
    RocksDbOutboxWriter, RocksDbSessionControl, RocksDbSessionState, RocksIndexOptions,
};
use tempfile::TempDir;

/// Helper: open a RocksDB database at the given path with a test query ID.
fn open_db(path: &str, query_id: &str) -> Arc<drasi_index_rocksdb::IndexDb> {
    let options = RocksIndexOptions::new(false, false, RocksDbMemoryBudget::default());
    open_unified_db(path, query_id, &options).expect("Failed to open RocksDB")
}

fn outbox_writer(db: Arc<drasi_index_rocksdb::IndexDb>) -> RocksDbOutboxWriter {
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    RocksDbOutboxWriter::new(db, session_state)
}

fn live_writer(db: Arc<drasi_index_rocksdb::IndexDb>) -> RocksDbLiveResultsWriter {
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    RocksDbLiveResultsWriter::new(db, session_state)
}

// ─── OutboxWriter Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_outbox_append_and_read() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);

    writer.append("q1", 1, b"hello").await.unwrap();
    writer.append("q1", 2, b"world").await.unwrap();
    writer.append("q1", 5, b"skip").await.unwrap();

    let entries = writer.read_from("q1", 0).await.unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], (1, b"hello".to_vec()));
    assert_eq!(entries[1], (2, b"world".to_vec()));
    assert_eq!(entries[2], (5, b"skip".to_vec()));

    // Read from middle
    let entries = writer.read_from("q1", 2).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], (5, b"skip".to_vec()));

    // Read from end
    let entries = writer.read_from("q1", 5).await.unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn test_outbox_read_latest_sequence() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);

    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), None);

    writer.append("q1", 10, b"data").await.unwrap();
    writer.append("q1", 20, b"data").await.unwrap();
    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), Some(20));

    writer.append("q1", 15, b"data").await.unwrap();
    // Latest should still be 20 (ordered by key)
    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), Some(20));
}

#[tokio::test]
async fn test_outbox_read_after_maximum_sequence_is_empty() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);
    writer.append("q1", u64::MAX, b"last").await.unwrap();
    assert_eq!(
        writer.read_from("q1", u64::MAX - 1).await.unwrap(),
        [(u64::MAX, b"last".to_vec())]
    );
    assert!(writer.read_from("q1", u64::MAX).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_provider_atomic_output_and_clear_share_the_index_session() {
    let tmp = TempDir::new().unwrap();
    let provider = RocksDbIndexProvider::new(tmp.path(), false, false);
    assert!(provider.supports_atomic_query_output());
    let resources = provider.create_indexes("q1").await.unwrap();
    let checkpoint = resources.checkpoint_store.as_ref().unwrap();
    let outbox = resources.outbox_writer.as_ref().unwrap();
    let live = resources.live_results_writer.as_ref().unwrap();
    {
        let _guard = SessionGuard::begin(resources.set.session_control.clone())
            .await
            .unwrap();
        checkpoint
            .stage_checkpoint("source", 1, None)
            .await
            .unwrap();
        checkpoint.stage_result_sequence("q1", 1).await.unwrap();
        outbox.append_and_trim("q1", 1, b"one", 1).await.unwrap();
        live.apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"row"),
            }],
        )
        .await
        .unwrap();
        assert_eq!(checkpoint.read_result_sequence("q1").await.unwrap(), None);
        assert!(outbox.read_from("q1", 0).await.unwrap().is_empty());
        assert!(live.read_snapshot("q1").await.unwrap().is_empty());
    }
    assert!(checkpoint
        .read_checkpoint("source")
        .await
        .unwrap()
        .is_none());
    assert_eq!(checkpoint.read_result_sequence("q1").await.unwrap(), None);
    assert!(outbox.read_from("q1", 0).await.unwrap().is_empty());
    assert!(live.read_snapshot("q1").await.unwrap().is_empty());
    let guard = SessionGuard::begin(resources.set.session_control.clone())
        .await
        .unwrap();
    checkpoint
        .stage_checkpoint("source", 2, None)
        .await
        .unwrap();
    checkpoint.stage_result_sequence("q1", 2).await.unwrap();
    outbox.append_and_trim("q1", 2, b"two", 2).await.unwrap();
    live.apply_mutations(
        "q1",
        &[RowMutation {
            row_signature: 2,
            data: Some(b"row"),
        }],
    )
    .await
    .unwrap();
    guard.commit().await.unwrap();
    {
        let _guard = SessionGuard::begin(resources.set.session_control.clone())
            .await
            .unwrap();
        outbox.clear("q1").await.unwrap();
        live.clear("q1").await.unwrap();
    }
    assert_eq!(
        checkpoint.read_result_sequence("q1").await.unwrap(),
        Some(2)
    );
    assert_eq!(
        outbox.read_from("q1", 0).await.unwrap(),
        [(2, b"two".to_vec())]
    );
    assert_eq!(
        live.read_snapshot("q1").await.unwrap(),
        [(2, b"row".to_vec())]
    );
    let guard = SessionGuard::begin(resources.set.session_control.clone())
        .await
        .unwrap();
    outbox.clear("q1").await.unwrap();
    live.clear("q1").await.unwrap();
    guard.commit().await.unwrap();
    assert!(outbox.read_from("q1", 0).await.unwrap().is_empty());
    assert!(live.read_snapshot("q1").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_outbox_clear() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);

    writer.append("q1", 1, b"data1").await.unwrap();
    writer.append("q1", 2, b"data2").await.unwrap();
    writer.clear("q1").await.unwrap();

    let entries = writer.read_from("q1", 0).await.unwrap();
    assert!(entries.is_empty());
    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), None);
}

#[tokio::test]
async fn test_outbox_trim_to_capacity() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);

    for i in 1..=10 {
        writer.append("q1", i, b"data").await.unwrap();
    }

    let removed = writer.trim_to_capacity("q1", 3).await.unwrap();
    assert_eq!(removed, 7);

    let entries = writer.read_from("q1", 0).await.unwrap();
    assert_eq!(entries.len(), 3);
    // Should keep the latest 3: 8, 9, 10
    assert_eq!(entries[0].0, 8);
    assert_eq!(entries[1].0, 9);
    assert_eq!(entries[2].0, 10);
}

#[tokio::test]
async fn test_outbox_trim_no_op() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);

    writer.append("q1", 1, b"data").await.unwrap();
    let removed = writer.trim_to_capacity("q1", 5).await.unwrap();
    assert_eq!(removed, 0);
}

#[tokio::test]
async fn test_outbox_isolation_between_queries() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);

    writer.append("q1", 1, b"q1-data").await.unwrap();
    writer.append("q2", 1, b"q2-data").await.unwrap();

    writer.clear("q1").await.unwrap();
    assert!(writer.read_from("q1", 0).await.unwrap().is_empty());
    assert_eq!(writer.read_from("q2", 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_outbox_persistence_across_reopen() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();

    // Write data
    {
        let db = open_db(&path, "q1");
        let writer = outbox_writer(db);
        writer.append("q1", 1, b"persisted").await.unwrap();
        writer.append("q1", 2, b"data").await.unwrap();
    }

    // Re-open and verify
    {
        let db = open_db(&path, "q1");
        let writer = outbox_writer(db);
        let entries = writer.read_from("q1", 0).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], (1, b"persisted".to_vec()));
        assert_eq!(entries[1], (2, b"data".to_vec()));
        assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), Some(2));
    }
}

// ─── LiveResultsWriter Tests ─────────────────────────────────────────────────

#[tokio::test]
async fn test_live_results_apply_upserts() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = live_writer(db);

    let mutations = vec![
        RowMutation {
            row_signature: 1,
            data: Some(b"row1"),
        },
        RowMutation {
            row_signature: 2,
            data: Some(b"row2"),
        },
    ];
    writer.apply_mutations("q1", &mutations).await.unwrap();

    assert_eq!(writer.row_count("q1").await.unwrap(), 2);
    let snapshot = writer.read_snapshot("q1").await.unwrap();
    assert_eq!(snapshot.len(), 2);
}

#[tokio::test]
async fn test_live_results_apply_delete() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = live_writer(db);

    writer
        .apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"row1"),
            }],
        )
        .await
        .unwrap();

    writer
        .apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: None,
            }],
        )
        .await
        .unwrap();

    assert_eq!(writer.row_count("q1").await.unwrap(), 0);
}

#[tokio::test]
async fn test_live_results_upsert_overwrites() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = live_writer(db);

    writer
        .apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"v1"),
            }],
        )
        .await
        .unwrap();
    writer
        .apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"v2"),
            }],
        )
        .await
        .unwrap();

    assert_eq!(writer.row_count("q1").await.unwrap(), 1);
    let snapshot = writer.read_snapshot("q1").await.unwrap();
    assert_eq!(snapshot[0].1, b"v2");
}

#[tokio::test]
async fn test_live_results_clear() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = live_writer(db);

    writer
        .apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"data"),
            }],
        )
        .await
        .unwrap();
    writer.clear("q1").await.unwrap();
    assert_eq!(writer.row_count("q1").await.unwrap(), 0);
    assert!(writer.read_snapshot("q1").await.unwrap().is_empty());
}

#[tokio::test]
async fn test_live_results_isolation_between_queries() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = live_writer(db);

    writer
        .apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"a"),
            }],
        )
        .await
        .unwrap();
    writer
        .apply_mutations(
            "q2",
            &[RowMutation {
                row_signature: 1,
                data: Some(b"b"),
            }],
        )
        .await
        .unwrap();

    writer.clear("q1").await.unwrap();
    assert_eq!(writer.row_count("q1").await.unwrap(), 0);
    assert_eq!(writer.row_count("q2").await.unwrap(), 1);
}

#[tokio::test]
async fn test_live_results_persistence_across_reopen() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();

    // Write data
    {
        let db = open_db(&path, "q1");
        let writer = live_writer(db);
        writer
            .apply_mutations(
                "q1",
                &[
                    RowMutation {
                        row_signature: 100,
                        data: Some(b"persisted-row"),
                    },
                    RowMutation {
                        row_signature: 200,
                        data: Some(b"another-row"),
                    },
                ],
            )
            .await
            .unwrap();
    }

    // Re-open and verify
    {
        let db = open_db(&path, "q1");
        let writer = live_writer(db);
        assert_eq!(writer.row_count("q1").await.unwrap(), 2);
        let snapshot = writer.read_snapshot("q1").await.unwrap();
        assert_eq!(snapshot.len(), 2);
        // Verify data content (rows returned in key order)
        let mut snapshot_sorted = snapshot.clone();
        snapshot_sorted.sort_by_key(|(sig, _)| *sig);
        assert_eq!(snapshot_sorted[0], (100, b"persisted-row".to_vec()));
        assert_eq!(snapshot_sorted[1], (200, b"another-row".to_vec()));
    }
}

#[tokio::test]
async fn test_live_results_atomic_batch() {
    // Verify that apply_mutations applies all mutations atomically
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = live_writer(db);

    // Insert 3 rows, delete 1, update 1 in a single batch
    writer
        .apply_mutations(
            "q1",
            &[
                RowMutation {
                    row_signature: 1,
                    data: Some(b"initial"),
                },
                RowMutation {
                    row_signature: 2,
                    data: Some(b"to-delete"),
                },
                RowMutation {
                    row_signature: 3,
                    data: Some(b"keep"),
                },
            ],
        )
        .await
        .unwrap();

    writer
        .apply_mutations(
            "q1",
            &[
                RowMutation {
                    row_signature: 1,
                    data: Some(b"updated"),
                },
                RowMutation {
                    row_signature: 2,
                    data: None,
                },
            ],
        )
        .await
        .unwrap();

    assert_eq!(writer.row_count("q1").await.unwrap(), 2);
    let snapshot = writer.read_snapshot("q1").await.unwrap();
    let mut snapshot_sorted = snapshot;
    snapshot_sorted.sort_by_key(|(sig, _)| *sig);
    assert_eq!(snapshot_sorted[0], (1, b"updated".to_vec()));
    assert_eq!(snapshot_sorted[1], (3, b"keep".to_vec()));
}

#[tokio::test]
async fn test_outbox_and_live_results_roll_back_with_session() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(RocksDbSessionControl::new(session_state.clone()));
    let outbox = RocksDbOutboxWriter::new(db.clone(), session_state.clone());
    let live = RocksDbLiveResultsWriter::new(db, session_state);

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        outbox.append("q1", 1, b"staged").await.unwrap();
        live.apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 7,
                data: Some(b"row"),
            }],
        )
        .await
        .unwrap();
        drop(guard);
    }

    assert!(outbox.read_from("q1", 0).await.unwrap().is_empty());
    assert_eq!(live.row_count("q1").await.unwrap(), 0);

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        outbox.append("q1", 1, b"committed").await.unwrap();
        live.apply_mutations(
            "q1",
            &[RowMutation {
                row_signature: 7,
                data: Some(b"row"),
            }],
        )
        .await
        .unwrap();
        guard.commit().await.expect("commit");
    }

    let entries = outbox.read_from("q1", 0).await.unwrap();
    assert_eq!(entries, vec![(1, b"committed".to_vec())]);
    assert_eq!(live.row_count("q1").await.unwrap(), 1);
}

#[tokio::test]
async fn test_outbox_trim_before_rolls_back_with_session() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(RocksDbSessionControl::new(session_state.clone()));
    let outbox = RocksDbOutboxWriter::new(db, session_state);

    for seq in 1..=4 {
        outbox.append("q1", seq, b"data").await.unwrap();
    }

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        outbox.append("q1", 5, b"five").await.unwrap();
        outbox.trim_before("q1", 4).await.unwrap();
        drop(guard);
    }

    let rolled_back: Vec<u64> = outbox
        .read_from("q1", 0)
        .await
        .unwrap()
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    assert_eq!(rolled_back, vec![1, 2, 3, 4]);

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        outbox.append("q1", 5, b"five").await.unwrap();
        let removed = outbox.trim_before("q1", 4).await.unwrap();
        assert_eq!(removed, 3);
        guard.commit().await.expect("commit");
    }

    let committed: Vec<u64> = outbox
        .read_from("q1", 0)
        .await
        .unwrap()
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    assert_eq!(committed, vec![4, 5]);
}

#[tokio::test]
async fn test_outbox_trim_before_survives_reopen() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();

    {
        let db = open_db(&path, "q1");
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        let session_control: Arc<dyn SessionControl> =
            Arc::new(RocksDbSessionControl::new(session_state.clone()));
        let outbox = RocksDbOutboxWriter::new(db, session_state);

        for seq in 1..=3 {
            outbox.append("q1", seq, b"data").await.unwrap();
        }

        let guard = SessionGuard::begin(session_control).await.expect("begin");
        outbox.append("q1", 4, b"four").await.unwrap();
        outbox.trim_before("q1", 3).await.unwrap();
        guard.commit().await.expect("commit");
    }

    {
        let db = open_db(&path, "q1");
        let writer = outbox_writer(db);
        let sequences: Vec<u64> = writer
            .read_from("q1", 0)
            .await
            .unwrap()
            .into_iter()
            .map(|(seq, _)| seq)
            .collect();
        assert_eq!(sequences, vec![3, 4]);
    }
}

fn outbox_sequences(entries: &[(u64, Vec<u8>)]) -> Vec<u64> {
    entries.iter().map(|(seq, _)| *seq).collect()
}

#[tokio::test]
async fn battle_trim_before_ring_holds_across_many_sessions() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();
    const CAPACITY: u64 = 3;
    const LAST: u64 = 40;

    {
        let db = open_db(&path, "q1");
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        let session_control: Arc<dyn SessionControl> =
            Arc::new(RocksDbSessionControl::new(session_state.clone()));
        let outbox = RocksDbOutboxWriter::new(db, session_state);

        for seq in 1..=LAST {
            let guard = SessionGuard::begin(session_control.clone())
                .await
                .expect("begin");
            outbox.append("q1", seq, b"data").await.unwrap();
            let retain_from = seq.saturating_sub(CAPACITY).saturating_add(1);
            outbox.trim_before("q1", retain_from).await.unwrap();
            guard.commit().await.expect("commit");

            let sequences = outbox_sequences(&outbox.read_from("q1", 0).await.unwrap());
            assert!(
                sequences.len() as u64 <= CAPACITY,
                "seq={seq} left {sequences:?}"
            );
            let expected: Vec<u64> = ((seq.saturating_sub(CAPACITY) + 1)..=seq).collect();
            assert_eq!(sequences, expected, "seq={seq}");
        }
    }

    let db = open_db(&path, "q1");
    let writer = outbox_writer(db);
    let sequences = outbox_sequences(&writer.read_from("q1", 0).await.unwrap());
    assert_eq!(sequences, vec![LAST - 2, LAST - 1, LAST]);
}

#[tokio::test]
async fn battle_trim_before_capacity_one() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(RocksDbSessionControl::new(session_state.clone()));
    let outbox = RocksDbOutboxWriter::new(db, session_state);

    for seq in 1..=8 {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        outbox.append("q1", seq, &[seq as u8]).await.unwrap();
        outbox.trim_before("q1", seq).await.unwrap();
        guard.commit().await.expect("commit");
        let sequences = outbox_sequences(&outbox.read_from("q1", 0).await.unwrap());
        assert_eq!(sequences, vec![seq], "capacity 1 must keep only the latest");
    }
}

#[tokio::test]
async fn battle_batched_appends_then_trim_in_one_session() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(RocksDbSessionControl::new(session_state.clone()));
    let outbox = RocksDbOutboxWriter::new(db, session_state);

    let guard = SessionGuard::begin(session_control).await.expect("begin");
    for seq in 1..=5 {
        outbox.append("q1", seq, b"data").await.unwrap();
    }
    outbox.trim_before("q1", 4).await.unwrap();
    guard.commit().await.expect("commit");

    let sequences = outbox_sequences(&outbox.read_from("q1", 0).await.unwrap());
    assert_eq!(
        sequences,
        vec![4, 5],
        "uncommitted appends below retain_from must leave with the committed ring"
    );
}

#[tokio::test]
async fn battle_trim_to_capacity_sees_uncommitted_appends() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(RocksDbSessionControl::new(session_state.clone()));
    let outbox = RocksDbOutboxWriter::new(db, session_state);

    let guard = SessionGuard::begin(session_control).await.expect("begin");
    for seq in 1..=5 {
        outbox.append("q1", seq, b"data").await.unwrap();
    }
    let removed = outbox.trim_to_capacity("q1", 2).await.unwrap();
    assert_eq!(removed, 3);
    guard.commit().await.expect("commit");

    let sequences = outbox_sequences(&outbox.read_from("q1", 0).await.unwrap());
    assert_eq!(sequences, vec![4, 5]);
}

#[tokio::test]
async fn battle_append_and_trim_rolls_back_with_session() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(RocksDbSessionControl::new(session_state.clone()));
    let outbox = RocksDbOutboxWriter::new(db, session_state);

    for seq in 1..=3 {
        outbox.append("q1", seq, b"old").await.unwrap();
    }

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        outbox.append_and_trim("q1", 4, b"four", 3).await.unwrap();
        drop(guard);
    }
    assert_eq!(
        outbox_sequences(&outbox.read_from("q1", 0).await.unwrap()),
        vec![1, 2, 3],
        "rolled-back append_and_trim must not evict committed keys"
    );

    {
        let guard = SessionGuard::begin(session_control).await.expect("begin");
        outbox.append_and_trim("q1", 4, b"four", 3).await.unwrap();
        guard.commit().await.expect("commit");
    }
    assert_eq!(
        outbox_sequences(&outbox.read_from("q1", 0).await.unwrap()),
        vec![3, 4]
    );
}

#[tokio::test]
async fn battle_trim_before_zero_is_noop() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = outbox_writer(db);
    writer.append("q1", 1, b"data").await.unwrap();
    assert_eq!(writer.trim_before("q1", 0).await.unwrap(), 0);
    assert_eq!(
        outbox_sequences(&writer.read_from("q1", 0).await.unwrap()),
        vec![1]
    );
}
