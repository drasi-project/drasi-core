// Copyright 2026 The Drasi Authors.
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

//! Regression tests for the per-CF write buffer and arena sizing policy.
//!
//! Every column family (including the mandatory `default` CF) must carry an
//! explicit `write_buffer_size` and `arena_block_size` per the tier policy in
//! `src/sizing.rs`, instead of RocksDB's defaults (64 MiB buffers, 1 MiB
//! sanitized arena blocks). The effective (post-sanitization) values are read
//! back from the OPTIONS file RocksDB writes at open, so this catches both a
//! CF the policy was never applied to and a silently-ignored value. The
//! drop/re-create paths behind `clear()` are covered too: a recreated CF must
//! keep the same sizing and retention bound it was opened with.

use std::collections::HashMap;
use std::sync::Arc;

use drasi_core::interface::{
    AccumulatorIndex, ElementArchiveIndex, ElementIndex, FutureQueue, IndexBackendPlugin,
};
use drasi_index_rocksdb::element_index::RocksDbElementIndex;
use drasi_index_rocksdb::future_queue::RocksDbFutureQueue;
use drasi_index_rocksdb::open_unified_db;
use drasi_index_rocksdb::result_index::RocksDbResultIndex;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_index_rocksdb::RocksDbSessionState;
use drasi_index_rocksdb::RocksIndexOptions;

const LARGE_BUFFER_CFS: &[&str] = &[
    "elements",
    "inbound",
    "outbound",
    "values",
    "archive",
    "outbox",
    "live_results",
];
const SMALL_BUFFER_CFS: &[&str] = &[
    "default",
    "slots",
    "partial",
    "sorted-sets",
    "metadata",
    "fqueue",
    "findex",
    "stream_state",
];
const LARGE_WRITE_BUFFER_SIZE: u64 = 16 * 1024 * 1024;
const SMALL_WRITE_BUFFER_SIZE: u64 = 8 * 1024 * 1024;

/// 1 MiB flushed-memtable history bound (WRITE_BUFFER_HISTORY_BYTES, kept
/// crate-private; update both together if the bound ever changes).
const HISTORY_BYTES: u64 = 1024 * 1024;

/// Effective `(write_buffer_size, arena_block_size,
/// max_write_buffer_size_to_maintain)` per CF, from the newest OPTIONS-* file
/// of the DB at `db_dir`.
fn effective_sizes(db_dir: &std::path::Path) -> HashMap<String, (u64, u64, u64)> {
    let options_file = std::fs::read_dir(db_dir)
        .expect("read db dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("OPTIONS-"))
        .max_by_key(|e| e.file_name().to_string_lossy().to_string())
        .expect("OPTIONS file present");
    let text = std::fs::read_to_string(options_file.path()).expect("read OPTIONS");

    let mut sizes: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut current_cf: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[CFOptions \"") {
            current_cf = rest.strip_suffix("\"]").map(str::to_string);
        } else if let Some(cf) = current_cf.clone() {
            if let Some(v) = line.strip_prefix("write_buffer_size=") {
                sizes.entry(cf).or_default().0 = v.parse().expect("wbs");
            } else if let Some(v) = line.strip_prefix("arena_block_size=") {
                sizes.entry(cf).or_default().1 = v.parse().expect("arena");
            } else if let Some(v) = line.strip_prefix("max_write_buffer_size_to_maintain=") {
                sizes.entry(cf).or_default().2 = v.parse().expect("maintain");
            }
        }
    }
    sizes
}

#[test]
fn effective_sizes_match_the_tier_policy_on_every_cf() {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = RocksIndexOptions::new(true, false);
    let db = open_unified_db(dir.path().to_str().unwrap(), "sizing-test", &options).expect("open");

    let sizes = effective_sizes(&dir.path().join("sizing-test"));
    assert_eq!(
        sizes.len(),
        LARGE_BUFFER_CFS.len() + SMALL_BUFFER_CFS.len(),
        "unexpected CF set: {:?}",
        sizes.keys().collect::<Vec<_>>()
    );

    for cf in LARGE_BUFFER_CFS {
        assert_eq!(
            sizes[*cf],
            (
                LARGE_WRITE_BUFFER_SIZE,
                LARGE_WRITE_BUFFER_SIZE / 64,
                HISTORY_BYTES
            ),
            "CF '{cf}'"
        );
    }
    for cf in SMALL_BUFFER_CFS {
        assert_eq!(
            sizes[*cf],
            (
                SMALL_WRITE_BUFFER_SIZE,
                SMALL_WRITE_BUFFER_SIZE / 64,
                HISTORY_BYTES
            ),
            "CF '{cf}'"
        );
    }

    drop(db);
}

#[tokio::test]
async fn provider_applies_the_internal_tier_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = RocksDbIndexProvider::new(dir.path(), true, false);
    let created = provider
        .create_indexes("provider-test")
        .await
        .expect("create indexes");

    let sizes = effective_sizes(&dir.path().join("provider-test"));
    assert_eq!(
        sizes["elements"],
        (
            LARGE_WRITE_BUFFER_SIZE,
            LARGE_WRITE_BUFFER_SIZE / 64,
            HISTORY_BYTES
        )
    );
    assert_eq!(
        sizes["stream_state"],
        (
            SMALL_WRITE_BUFFER_SIZE,
            SMALL_WRITE_BUFFER_SIZE / 64,
            HISTORY_BYTES
        )
    );

    drop(created);
}

#[tokio::test]
async fn clear_recreates_cfs_with_the_sizing_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = RocksIndexOptions::new(true, false);
    let db = open_unified_db(dir.path().to_str().unwrap(), "clear-test", &options).expect("open");

    let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
    let element_index = RocksDbElementIndex::new(db.clone(), options, session_state.clone());
    let result_index = RocksDbResultIndex::new(db.clone(), session_state.clone(), options);
    let future_queue = RocksDbFutureQueue::new(db.clone(), session_state.clone(), options);

    // RocksDbElementIndex implements both ElementIndex and ElementArchiveIndex
    // (each with a clear()), so call them fully qualified. Between them they
    // re-create elements, slots, inbound, outbound, partial, and archive.
    ElementIndex::clear(&element_index)
        .await
        .expect("clear element index");
    ElementArchiveIndex::clear(&element_index)
        .await
        .expect("clear archive index");
    result_index.clear().await.expect("clear result index");
    future_queue.clear().await.expect("clear future queue");

    // create_cf persists a fresh OPTIONS file; the recreated CFs must retain
    // the internal tier sizing and history bound rather than RocksDB defaults.
    let large = (
        LARGE_WRITE_BUFFER_SIZE,
        LARGE_WRITE_BUFFER_SIZE / 64,
        HISTORY_BYTES,
    );
    let small = (
        SMALL_WRITE_BUFFER_SIZE,
        SMALL_WRITE_BUFFER_SIZE / 64,
        HISTORY_BYTES,
    );
    let sizes = effective_sizes(&dir.path().join("clear-test"));
    for cf in ["elements", "inbound", "outbound", "values", "archive"] {
        assert_eq!(sizes[cf], large, "CF '{cf}'");
    }
    for cf in ["slots", "partial", "sorted-sets", "fqueue", "findex"] {
        assert_eq!(sizes[cf], small, "CF '{cf}'");
    }

    drop(db);
}
