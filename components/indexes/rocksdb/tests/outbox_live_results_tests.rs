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

use drasi_core::interface::{LiveResultsWriter, OutboxWriter, RowMutation};
use drasi_index_rocksdb::{
    element_index::RocksIndexOptions, open_unified_db, RocksDbLiveResultsWriter,
    RocksDbOutboxWriter,
};
use tempfile::TempDir;

/// Helper: open a RocksDB database at the given path with a test query ID.
fn open_db(path: &str, query_id: &str) -> Arc<rocksdb::OptimisticTransactionDB> {
    let options = RocksIndexOptions {
        archive_enabled: false,
        direct_io: false,
    };
    open_unified_db(path, query_id, &options).expect("Failed to open RocksDB")
}

// ─── OutboxWriter Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_outbox_append_and_read() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = RocksDbOutboxWriter::new(db);

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
    let writer = RocksDbOutboxWriter::new(db);

    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), None);

    writer.append("q1", 10, b"data").await.unwrap();
    writer.append("q1", 20, b"data").await.unwrap();
    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), Some(20));

    writer.append("q1", 15, b"data").await.unwrap();
    // Latest should still be 20 (ordered by key)
    assert_eq!(writer.read_latest_sequence("q1").await.unwrap(), Some(20));
}

#[tokio::test]
async fn test_outbox_clear() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = RocksDbOutboxWriter::new(db);

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
    let writer = RocksDbOutboxWriter::new(db);

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
    let writer = RocksDbOutboxWriter::new(db);

    writer.append("q1", 1, b"data").await.unwrap();
    let removed = writer.trim_to_capacity("q1", 5).await.unwrap();
    assert_eq!(removed, 0);
}

#[tokio::test]
async fn test_outbox_isolation_between_queries() {
    let tmp = TempDir::new().unwrap();
    let db = open_db(tmp.path().to_str().unwrap(), "q1");
    let writer = RocksDbOutboxWriter::new(db);

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
        let writer = RocksDbOutboxWriter::new(db);
        writer.append("q1", 1, b"persisted").await.unwrap();
        writer.append("q1", 2, b"data").await.unwrap();
    }

    // Re-open and verify
    {
        let db = open_db(&path, "q1");
        let writer = RocksDbOutboxWriter::new(db);
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
    let writer = RocksDbLiveResultsWriter::new(db);

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
    let writer = RocksDbLiveResultsWriter::new(db);

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
    let writer = RocksDbLiveResultsWriter::new(db);

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
    let writer = RocksDbLiveResultsWriter::new(db);

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
    let writer = RocksDbLiveResultsWriter::new(db);

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
        let writer = RocksDbLiveResultsWriter::new(db);
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
        let writer = RocksDbLiveResultsWriter::new(db);
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
    let writer = RocksDbLiveResultsWriter::new(db);

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
