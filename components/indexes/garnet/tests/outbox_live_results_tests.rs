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

//! Integration tests for Garnet/Redis OutboxWriter and LiveResultsWriter implementations.
//!
//! Requires a running Redis instance. Uses testcontainers via `shared_tests::redis_helpers`.
//! Tests are marked `#[ignore]` for CI environments without Docker.

use std::sync::Arc;

use drasi_core::interface::{
    IndexBackendPlugin, LiveResultsWriter, OutboxWriter, RowMutation, SessionControl, SessionGuard,
};
use drasi_index_garnet::{
    GarnetIndexProvider, GarnetLiveResultsWriter, GarnetOutboxWriter, GarnetSessionControl,
    GarnetSessionState,
};
use shared_tests::redis_helpers::{setup_redis, RedisGuard};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// Shared Redis container for all tests in this file.
static SHARED_REDIS: OnceCell<RedisGuard> = OnceCell::const_new();

async fn shared_redis() -> &'static RedisGuard {
    SHARED_REDIS
        .get_or_init(|| async { setup_redis().await })
        .await
}

/// Get a Redis connection from the shared container.
async fn get_connection() -> redis::aio::MultiplexedConnection {
    let guard = shared_redis().await;
    let client = redis::Client::open(guard.url()).unwrap();
    client.get_multiplexed_async_connection().await.unwrap()
}

/// Generate a unique query ID to avoid test interference.
fn unique_query_id() -> String {
    format!("test-{}", Uuid::new_v4())
}

// ─── OutboxWriter Tests ──────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_append_and_read() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetOutboxWriter::new(&qid, con);

    writer.append(&qid, 1, b"hello").await.unwrap();
    writer.append(&qid, 2, b"world").await.unwrap();
    writer.append(&qid, 5, b"skip").await.unwrap();

    let entries = writer.read_from(&qid, 0).await.unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], (1, b"hello".to_vec()));
    assert_eq!(entries[1], (2, b"world".to_vec()));
    assert_eq!(entries[2], (5, b"skip".to_vec()));

    // Read from middle
    let entries = writer.read_from(&qid, 2).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0], (5, b"skip".to_vec()));

    // Read from end
    let entries = writer.read_from(&qid, 5).await.unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_read_latest_sequence() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetOutboxWriter::new(&qid, con);

    assert_eq!(writer.read_latest_sequence(&qid).await.unwrap(), None);

    writer.append(&qid, 10, b"data").await.unwrap();
    writer.append(&qid, 20, b"data").await.unwrap();
    assert_eq!(writer.read_latest_sequence(&qid).await.unwrap(), Some(20));
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_clear() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetOutboxWriter::new(&qid, con);

    writer.append(&qid, 1, b"data1").await.unwrap();
    writer.append(&qid, 2, b"data2").await.unwrap();
    writer.clear(&qid).await.unwrap();

    let entries = writer.read_from(&qid, 0).await.unwrap();
    assert!(entries.is_empty());
    assert_eq!(writer.read_latest_sequence(&qid).await.unwrap(), None);
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_trim_to_capacity() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetOutboxWriter::new(&qid, con);

    for i in 1..=10 {
        writer.append(&qid, i, b"data").await.unwrap();
    }

    let removed = writer.trim_to_capacity(&qid, 3).await.unwrap();
    assert_eq!(removed, 7);

    let entries = writer.read_from(&qid, 0).await.unwrap();
    assert_eq!(entries.len(), 3);
    // Should keep the latest 3: 8, 9, 10
    assert_eq!(entries[0].0, 8);
    assert_eq!(entries[1].0, 9);
    assert_eq!(entries[2].0, 10);
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_trim_before() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetOutboxWriter::new(&qid, con);

    for i in 1..=5 {
        writer.append(&qid, i, b"data").await.unwrap();
    }

    let removed = writer.trim_before(&qid, 4).await.unwrap();
    assert_eq!(removed, 3);

    let entries = writer.read_from(&qid, 0).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, 4);
    assert_eq!(entries[1].0, 5);
}

fn outbox_sequences(entries: &[(u64, Vec<u8>)]) -> Vec<u64> {
    entries.iter().map(|(seq, _)| *seq).collect()
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_trim_before_without_active_session() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let direct = GarnetOutboxWriter::new(&qid, con.clone());
    for i in 1..=5 {
        direct.append(&qid, i, b"data").await.unwrap();
    }

    let session_state = Arc::new(GarnetSessionState::new(con.clone()));
    let writer = GarnetOutboxWriter::new(&qid, con).with_session_state(session_state);
    let removed = writer.trim_before(&qid, 4).await.unwrap();
    assert_eq!(removed, 3);
    assert_eq!(
        outbox_sequences(&writer.read_from(&qid, 0).await.unwrap()),
        vec![4, 5]
    );
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_trim_before_in_session_rolls_back() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let session_state = Arc::new(GarnetSessionState::new(con.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(GarnetSessionControl::new(session_state.clone()));
    let writer = GarnetOutboxWriter::new(&qid, con).with_session_state(session_state);

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        for seq in 1..=4 {
            writer.append(&qid, seq, b"data").await.unwrap();
        }
        guard.commit().await.expect("commit");
    }

    {
        let guard = SessionGuard::begin(session_control.clone())
            .await
            .expect("begin");
        writer.append(&qid, 5, b"five").await.unwrap();
        writer.trim_before(&qid, 4).await.unwrap();
        drop(guard);
    }
    assert_eq!(
        outbox_sequences(&writer.read_from(&qid, 0).await.unwrap()),
        vec![1, 2, 3, 4]
    );

    {
        let guard = SessionGuard::begin(session_control).await.expect("begin");
        writer.append(&qid, 5, b"five").await.unwrap();
        writer.trim_before(&qid, 4).await.unwrap();
        guard.commit().await.expect("commit");
    }
    assert_eq!(
        outbox_sequences(&writer.read_from(&qid, 0).await.unwrap()),
        vec![4, 5]
    );
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_trim_to_capacity_sees_uncommitted_appends() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let session_state = Arc::new(GarnetSessionState::new(con.clone()));
    let session_control: Arc<dyn SessionControl> =
        Arc::new(GarnetSessionControl::new(session_state.clone()));
    let writer = GarnetOutboxWriter::new(&qid, con).with_session_state(session_state);

    let guard = SessionGuard::begin(session_control).await.expect("begin");
    for seq in 1..=5 {
        writer.append(&qid, seq, b"data").await.unwrap();
    }
    let removed = writer.trim_to_capacity(&qid, 2).await.unwrap();
    assert_eq!(removed, 3);
    guard.commit().await.expect("commit");
    assert_eq!(
        outbox_sequences(&writer.read_from(&qid, 0).await.unwrap()),
        vec![4, 5]
    );
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_trim_no_op() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetOutboxWriter::new(&qid, con);

    writer.append(&qid, 1, b"data").await.unwrap();
    let removed = writer.trim_to_capacity(&qid, 5).await.unwrap();
    assert_eq!(removed, 0);
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_isolation_between_queries() {
    let con = get_connection().await;
    let qid1 = unique_query_id();
    let qid2 = unique_query_id();
    let writer1 = GarnetOutboxWriter::new(&qid1, con.clone());
    let writer2 = GarnetOutboxWriter::new(&qid2, con);

    writer1.append(&qid1, 1, b"q1-data").await.unwrap();
    writer2.append(&qid2, 1, b"q2-data").await.unwrap();

    writer1.clear(&qid1).await.unwrap();
    assert!(writer1.read_from(&qid1, 0).await.unwrap().is_empty());
    assert_eq!(writer2.read_from(&qid2, 0).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore]
async fn test_garnet_output_scope_binding_preserves_primary_keys() {
    let connection = get_connection().await;
    let storage_scope = unique_query_id();
    let query_id = "logical-query";
    let auxiliary_id = "metadata:{logical-query}";
    let outbox =
        GarnetOutboxWriter::new(&storage_scope, connection.clone()).with_query_id(query_id);
    let live =
        GarnetLiveResultsWriter::new(&storage_scope, connection.clone()).with_query_id(query_id);
    outbox.append(query_id, 7, b"primary").await.unwrap();
    outbox.append(auxiliary_id, 1, b"metadata").await.unwrap();
    live.apply_mutations(
        query_id,
        &[RowMutation {
            row_signature: 7,
            data: Some(b"primary"),
        }],
    )
    .await
    .unwrap();
    live.apply_mutations(
        auxiliary_id,
        &[RowMutation {
            row_signature: 1,
            data: Some(b"metadata"),
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        outbox.read_from(query_id, 0).await.unwrap(),
        [(7, b"primary".to_vec())]
    );
    assert_eq!(
        outbox.read_from(auxiliary_id, 0).await.unwrap(),
        [(1, b"metadata".to_vec())]
    );
    assert_eq!(
        live.read_snapshot(query_id).await.unwrap(),
        [(7, b"primary".to_vec())]
    );
    assert_eq!(
        live.read_snapshot(auxiliary_id).await.unwrap(),
        [(1, b"metadata".to_vec())]
    );

    let legacy_outbox = GarnetOutboxWriter::new(&storage_scope, connection.clone());
    let legacy_live = GarnetLiveResultsWriter::new(&storage_scope, connection);
    assert_eq!(
        legacy_outbox.read_from(&storage_scope, 0).await.unwrap(),
        [(7, b"primary".to_vec())]
    );
    assert_eq!(
        legacy_live.read_snapshot(&storage_scope).await.unwrap(),
        [(7, b"primary".to_vec())]
    );
    outbox.clear(auxiliary_id).await.unwrap();
    live.clear(auxiliary_id).await.unwrap();
    assert!(outbox.read_from(auxiliary_id, 0).await.unwrap().is_empty());
    assert!(live.read_snapshot(auxiliary_id).await.unwrap().is_empty());
    assert_eq!(
        outbox.read_latest_sequence(query_id).await.unwrap(),
        Some(7)
    );
    assert_eq!(live.row_count(query_id).await.unwrap(), 1);
    outbox.clear(query_id).await.unwrap();
    live.clear(query_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_garnet_outbox_sequence_order_is_exact_above_double_precision() {
    let connection = get_connection().await;
    let query_id = unique_query_id();
    let outbox = GarnetOutboxWriter::new(&query_id, connection);
    let sequences = [
        (1 << 53) - 1,
        1 << 53,
        (1 << 53) + 1,
        9_999_999_999_999_999,
        10_000_000_000_000_000,
        u64::MAX - 1,
        u64::MAX,
    ];
    for sequence in sequences {
        outbox.append(&query_id, sequence, b"entry").await.unwrap();
        assert_eq!(
            outbox.read_latest_sequence(&query_id).await.unwrap(),
            Some(sequence)
        );
    }
    for after in sequences {
        assert_eq!(
            outbox_sequences(&outbox.read_from(&query_id, after).await.unwrap()),
            sequences
                .into_iter()
                .filter(|seq| *seq > after)
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(
        outbox.trim_to_capacity(&query_id, 2).await.unwrap(),
        sequences.len() - 2
    );
    assert_eq!(
        outbox_sequences(&outbox.read_from(&query_id, 0).await.unwrap()),
        [u64::MAX - 1, u64::MAX]
    );
    outbox.clear(&query_id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_garnet_scoped_provider_binds_output_and_commits_all_resources_together() {
    let redis = shared_redis().await;
    let storage_scope = unique_query_id();
    let provider: Arc<dyn IndexBackendPlugin> =
        Arc::new(GarnetIndexProvider::new(redis.url(), None, false));
    assert!(provider.supports_atomic_query_output());
    let query_id = "logical-query";
    let metadata_id = "metadata:{logical-query}";
    {
        let resources = provider
            .create_scoped_indexes(&storage_scope, query_id)
            .await
            .unwrap();
        let checkpoint = resources.checkpoint_store.as_ref().unwrap();
        let outbox = resources.outbox_writer.as_ref().unwrap();
        let live = resources.live_results_writer.as_ref().unwrap();
        {
            let _session = SessionGuard::begin(resources.set.session_control.clone())
                .await
                .unwrap();
            checkpoint
                .stage_checkpoint("source", 1, None)
                .await
                .unwrap();
            checkpoint.stage_result_sequence(query_id, 1).await.unwrap();
            outbox
                .append_and_trim(query_id, 1, b"discarded", 1)
                .await
                .unwrap();
            outbox
                .append(metadata_id, 1, b"discarded-metadata")
                .await
                .unwrap();
            live.apply_mutations(
                query_id,
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"discarded"),
                }],
            )
            .await
            .unwrap();
            assert_eq!(
                checkpoint.read_result_sequence(query_id).await.unwrap(),
                None
            );
        }
        assert_eq!(
            checkpoint.read_result_sequence(query_id).await.unwrap(),
            None
        );
        assert!(checkpoint
            .read_checkpoint("source")
            .await
            .unwrap()
            .is_none());
        assert!(outbox.read_from(query_id, 0).await.unwrap().is_empty());
        assert!(outbox.read_from(metadata_id, 0).await.unwrap().is_empty());
        assert!(live.read_snapshot(query_id).await.unwrap().is_empty());
        let session = SessionGuard::begin(resources.set.session_control.clone())
            .await
            .unwrap();
        checkpoint
            .stage_checkpoint("source", 2, None)
            .await
            .unwrap();
        checkpoint.stage_result_sequence(query_id, 2).await.unwrap();
        outbox
            .append_and_trim(query_id, 2, b"primary", 2)
            .await
            .unwrap();
        outbox.append(metadata_id, 1, b"metadata").await.unwrap();
        live.apply_mutations(
            query_id,
            &[RowMutation {
                row_signature: 2,
                data: Some(b"primary"),
            }],
        )
        .await
        .unwrap();
        session.commit().await.unwrap();
        checkpoint
            .write_output_generation(query_id, 3)
            .await
            .unwrap();
    }
    let reopened = provider
        .create_scoped_indexes(&storage_scope, query_id)
        .await
        .unwrap();
    let checkpoint = reopened.checkpoint_store.as_ref().unwrap();
    assert_eq!(
        checkpoint.read_result_sequence(query_id).await.unwrap(),
        Some(2)
    );
    assert_eq!(
        checkpoint
            .read_checkpoint("source")
            .await
            .unwrap()
            .unwrap()
            .sequence,
        2
    );
    assert_eq!(
        checkpoint.read_output_generation(query_id).await.unwrap(),
        Some(3)
    );
    assert_eq!(
        reopened
            .outbox_writer
            .as_ref()
            .unwrap()
            .read_from(query_id, 0)
            .await
            .unwrap(),
        [(2, b"primary".to_vec())]
    );
    assert_eq!(
        reopened
            .outbox_writer
            .as_ref()
            .unwrap()
            .read_from(metadata_id, 0)
            .await
            .unwrap(),
        [(1, b"metadata".to_vec())]
    );
    assert_eq!(
        reopened
            .live_results_writer
            .as_ref()
            .unwrap()
            .read_snapshot(query_id)
            .await
            .unwrap(),
        [(2, b"primary".to_vec())]
    );
    let legacy_reader = provider.create_indexes(&storage_scope).await.unwrap();
    assert_eq!(
        legacy_reader
            .outbox_writer
            .as_ref()
            .unwrap()
            .read_from(&storage_scope, 0)
            .await
            .unwrap(),
        [(2, b"primary".to_vec())]
    );
    assert_eq!(
        legacy_reader
            .live_results_writer
            .as_ref()
            .unwrap()
            .read_snapshot(&storage_scope)
            .await
            .unwrap(),
        [(2, b"primary".to_vec())]
    );
    checkpoint.clear_checkpoints().await.unwrap();
    assert_eq!(
        checkpoint.read_result_sequence(query_id).await.unwrap(),
        Some(2)
    );
    assert_eq!(
        checkpoint.read_output_generation(query_id).await.unwrap(),
        Some(3)
    );
}

// ─── LiveResultsWriter Tests ─────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn test_garnet_live_results_apply_upserts() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetLiveResultsWriter::new(&qid, con);

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
    writer.apply_mutations(&qid, &mutations).await.unwrap();

    assert_eq!(writer.row_count(&qid).await.unwrap(), 2);
    let snapshot = writer.read_snapshot(&qid).await.unwrap();
    assert_eq!(snapshot.len(), 2);
}

#[tokio::test]
#[ignore]
async fn test_garnet_live_results_apply_delete() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetLiveResultsWriter::new(&qid, con);

    writer
        .apply_mutations(
            &qid,
            &[RowMutation {
                row_signature: 1,
                data: Some(b"row1"),
            }],
        )
        .await
        .unwrap();

    writer
        .apply_mutations(
            &qid,
            &[RowMutation {
                row_signature: 1,
                data: None,
            }],
        )
        .await
        .unwrap();

    assert_eq!(writer.row_count(&qid).await.unwrap(), 0);
}

#[tokio::test]
#[ignore]
async fn test_garnet_live_results_upsert_overwrites() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetLiveResultsWriter::new(&qid, con);

    writer
        .apply_mutations(
            &qid,
            &[RowMutation {
                row_signature: 1,
                data: Some(b"v1"),
            }],
        )
        .await
        .unwrap();
    writer
        .apply_mutations(
            &qid,
            &[RowMutation {
                row_signature: 1,
                data: Some(b"v2"),
            }],
        )
        .await
        .unwrap();

    assert_eq!(writer.row_count(&qid).await.unwrap(), 1);
    let snapshot = writer.read_snapshot(&qid).await.unwrap();
    assert_eq!(snapshot[0].1, b"v2");
}

#[tokio::test]
#[ignore]
async fn test_garnet_live_results_clear() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetLiveResultsWriter::new(&qid, con);

    writer
        .apply_mutations(
            &qid,
            &[RowMutation {
                row_signature: 1,
                data: Some(b"data"),
            }],
        )
        .await
        .unwrap();
    writer.clear(&qid).await.unwrap();
    assert_eq!(writer.row_count(&qid).await.unwrap(), 0);
    assert!(writer.read_snapshot(&qid).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn test_garnet_live_results_isolation_between_queries() {
    let con = get_connection().await;
    let qid1 = unique_query_id();
    let qid2 = unique_query_id();
    let writer1 = GarnetLiveResultsWriter::new(&qid1, con.clone());
    let writer2 = GarnetLiveResultsWriter::new(&qid2, con);

    writer1
        .apply_mutations(
            &qid1,
            &[RowMutation {
                row_signature: 1,
                data: Some(b"a"),
            }],
        )
        .await
        .unwrap();
    writer2
        .apply_mutations(
            &qid2,
            &[RowMutation {
                row_signature: 1,
                data: Some(b"b"),
            }],
        )
        .await
        .unwrap();

    writer1.clear(&qid1).await.unwrap();
    assert_eq!(writer1.row_count(&qid1).await.unwrap(), 0);
    assert_eq!(writer2.row_count(&qid2).await.unwrap(), 1);
}

#[tokio::test]
#[ignore]
async fn test_garnet_live_results_atomic_batch() {
    let con = get_connection().await;
    let qid = unique_query_id();
    let writer = GarnetLiveResultsWriter::new(&qid, con);

    // Insert 3 rows
    writer
        .apply_mutations(
            &qid,
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

    // Update 1, delete 1 in single batch
    writer
        .apply_mutations(
            &qid,
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

    assert_eq!(writer.row_count(&qid).await.unwrap(), 2);
    let snapshot = writer.read_snapshot(&qid).await.unwrap();
    let mut snapshot_sorted = snapshot;
    snapshot_sorted.sort_by_key(|(sig, _)| *sig);
    assert_eq!(snapshot_sorted[0], (1, b"updated".to_vec()));
    assert_eq!(snapshot_sorted[1], (3, b"keep".to_vec()));
}
