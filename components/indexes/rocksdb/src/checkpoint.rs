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

//! RocksDB checkpoint writer for source sequence tracking and config hashing.
//!
//! Implements [`CheckpointWriter`] backed by a dedicated `stream_state` column
//! family. `stage_checkpoint` writes go through the shared
//! [`RocksDbSessionState`] so they land in the same transaction as index updates;
//! all other operations are standalone.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{CheckpointWriter, IndexError};
use rocksdb::{ColumnFamilyDescriptor, OptimisticTransactionDB, Options};
use tokio::task;

use crate::RocksDbSessionState;

/// Column family that stores source checkpoints and the config hash.
///
/// Kept separate from the graph CFs (`elements`, `values`, etc.) because
/// sequence numbers are overwritten on every event — a high-frequency overwrite
/// pattern that would otherwise trigger unnecessary compactions of graph state.
pub const STREAM_STATE_CF: &str = "stream_state";

const SOURCE_SEQUENCE_PREFIX: &[u8] = b"source_sequence:";
const CONFIG_HASH_KEY: &[u8] = b"config_hash";

/// Column family options for `stream_state`.
///
/// Optimized for point lookup of small fixed-size values (8-byte u64s).
/// Mirrors the `metadata` CF in [`super::result_index::get_metadata_cf_options`].
pub(crate) fn get_stream_state_cf_options() -> Options {
    let mut opts = Options::default();
    opts.optimize_for_point_lookup(1);
    opts
}

/// Column family descriptor for `stream_state`.
pub(crate) fn stream_state_cf_descriptors() -> Vec<ColumnFamilyDescriptor> {
    vec![ColumnFamilyDescriptor::new(
        STREAM_STATE_CF,
        get_stream_state_cf_options(),
    )]
}

/// RocksDB implementation of [`CheckpointWriter`].
///
/// Holds an `Arc<OptimisticTransactionDB>` for direct (non-session) reads and
/// writes, and an `Arc<RocksDbSessionState>` for staging into the active
/// session transaction.
pub(crate) struct RocksDbCheckpointWriter {
    db: Arc<OptimisticTransactionDB>,
    session_state: Arc<RocksDbSessionState>,
}

impl RocksDbCheckpointWriter {
    pub(crate) fn new(
        db: Arc<OptimisticTransactionDB>,
        session_state: Arc<RocksDbSessionState>,
    ) -> Self {
        Self { db, session_state }
    }
}

fn source_sequence_key(source_id: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(SOURCE_SEQUENCE_PREFIX.len() + source_id.len());
    key.extend_from_slice(SOURCE_SEQUENCE_PREFIX);
    key.extend_from_slice(source_id.as_bytes());
    key
}

fn decode_u64(bytes: &[u8]) -> Result<u64, IndexError> {
    let arr: [u8; 8] = bytes.try_into().map_err(|_| IndexError::CorruptedData)?;
    Ok(u64::from_be_bytes(arr))
}

#[async_trait]
impl CheckpointWriter for RocksDbCheckpointWriter {
    async fn stage_checkpoint(&self, source_id: &str, sequence: u64) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();
        let key = source_sequence_key(source_id);
        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");
            session_state.with_txn(|txn| {
                txn.put_cf(&cf, &key, sequence.to_be_bytes())
                    .map_err(IndexError::other)
            })
        });
        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_checkpoint(&self, source_id: &str) -> Result<Option<u64>, IndexError> {
        let db = self.db.clone();
        let key = source_sequence_key(source_id);
        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");
            match db.get_cf(&cf, &key) {
                Ok(Some(bytes)) => Ok(Some(decode_u64(&bytes)?)),
                Ok(None) => Ok(None),
                Err(e) => Err(IndexError::other(e)),
            }
        });
        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, u64>, IndexError> {
        let db = self.db.clone();
        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");
            let mut out = HashMap::new();
            for item in db.prefix_iterator_cf(&cf, SOURCE_SEQUENCE_PREFIX) {
                let (raw_key, raw_value) = item.map_err(IndexError::other)?;
                // prefix_iterator_cf may yield keys past the prefix; filter strictly.
                if !raw_key.starts_with(SOURCE_SEQUENCE_PREFIX) {
                    break;
                }
                let source_id = std::str::from_utf8(&raw_key[SOURCE_SEQUENCE_PREFIX.len()..])
                    .map_err(IndexError::other)?
                    .to_string();
                let sequence = decode_u64(&raw_value)?;
                out.insert(source_id, sequence);
            }
            Ok(out)
        });
        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let db = self.db.clone();
        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");
            let txn = db.transaction();
            // Collect first so the iterator borrow ends before delete_cf calls.
            let mut keys: Vec<Vec<u8>> = Vec::new();
            for item in db.prefix_iterator_cf(&cf, SOURCE_SEQUENCE_PREFIX) {
                let (raw_key, _) = item.map_err(IndexError::other)?;
                if !raw_key.starts_with(SOURCE_SEQUENCE_PREFIX) {
                    break;
                }
                keys.push(raw_key.to_vec());
            }
            for k in keys {
                txn.delete_cf(&cf, &k).map_err(IndexError::other)?;
            }
            txn.delete_cf(&cf, CONFIG_HASH_KEY)
                .map_err(IndexError::other)?;
            txn.commit().map_err(IndexError::other)
        });
        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let db = self.db.clone();
        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");
            db.put_cf(&cf, CONFIG_HASH_KEY, hash.to_be_bytes())
                .map_err(IndexError::other)
        });
        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        let db = self.db.clone();
        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");
            match db.get_cf(&cf, CONFIG_HASH_KEY) {
                Ok(Some(bytes)) => Ok(Some(decode_u64(&bytes)?)),
                Ok(None) => Ok(None),
                Err(e) => Err(IndexError::other(e)),
            }
        });
        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::open_unified_db;
    use crate::{element_index::RocksIndexOptions, RocksDbSessionControl};
    use drasi_core::interface::SessionControl;
    use tempfile::TempDir;

    fn open_writer(
        temp: &TempDir,
        query_id: &str,
    ) -> (RocksDbCheckpointWriter, Arc<RocksDbSessionControl>) {
        let path = temp.path().to_string_lossy().to_string();
        let options = RocksIndexOptions {
            archive_enabled: false,
            direct_io: false,
        };
        let db = open_unified_db(&path, query_id, &options).expect("open db");
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        let session_control = Arc::new(RocksDbSessionControl::new(session_state.clone()));
        let writer = RocksDbCheckpointWriter::new(db, session_state);
        (writer, session_control)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_then_read_after_commit() {
        let temp = TempDir::new().unwrap();
        let (writer, sc) = open_writer(&temp, "q1");

        sc.begin().await.unwrap();
        writer.stage_checkpoint("source-a", 42).await.unwrap();
        sc.commit().await.unwrap();

        assert_eq!(writer.read_checkpoint("source-a").await.unwrap(), Some(42));
        assert!(writer.read_checkpoint("source-b").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollback_discards_staged_checkpoint() {
        let temp = TempDir::new().unwrap();
        let (writer, sc) = open_writer(&temp, "q1");

        sc.begin().await.unwrap();
        writer.stage_checkpoint("source-a", 100).await.unwrap();
        sc.rollback().unwrap();

        assert!(writer.read_checkpoint("source-a").await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stage_without_session_errors() {
        let temp = TempDir::new().unwrap();
        let (writer, _sc) = open_writer(&temp, "q1");

        let err = writer.stage_checkpoint("source-a", 1).await;
        assert!(
            err.is_err(),
            "stage_checkpoint without session should error"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_all_returns_every_source() {
        let temp = TempDir::new().unwrap();
        let (writer, sc) = open_writer(&temp, "q1");

        sc.begin().await.unwrap();
        writer.stage_checkpoint("source-a", 10).await.unwrap();
        writer.stage_checkpoint("source-b", 20).await.unwrap();
        writer.stage_checkpoint("source-c", 30).await.unwrap();
        sc.commit().await.unwrap();

        let all = writer.read_all_checkpoints().await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all.get("source-a"), Some(&10));
        assert_eq!(all.get("source-b"), Some(&20));
        assert_eq!(all.get("source-c"), Some(&30));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_hash_round_trip() {
        let temp = TempDir::new().unwrap();
        let (writer, _sc) = open_writer(&temp, "q1");

        assert!(writer.read_config_hash().await.unwrap().is_none());
        writer.write_config_hash(0xDEAD_BEEF).await.unwrap();
        assert_eq!(writer.read_config_hash().await.unwrap(), Some(0xDEAD_BEEF));

        writer.write_config_hash(0x1234_5678).await.unwrap();
        assert_eq!(writer.read_config_hash().await.unwrap(), Some(0x1234_5678));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_checkpoints_wipes_everything() {
        let temp = TempDir::new().unwrap();
        let (writer, sc) = open_writer(&temp, "q1");

        sc.begin().await.unwrap();
        writer.stage_checkpoint("source-a", 1).await.unwrap();
        writer.stage_checkpoint("source-b", 2).await.unwrap();
        sc.commit().await.unwrap();
        writer.write_config_hash(99).await.unwrap();

        writer.clear_checkpoints().await.unwrap();

        assert!(writer.read_checkpoint("source-a").await.unwrap().is_none());
        assert!(writer.read_checkpoint("source-b").await.unwrap().is_none());
        assert!(writer.read_all_checkpoints().await.unwrap().is_empty());
        assert!(writer.read_config_hash().await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoints_survive_db_reopen() {
        let temp = TempDir::new().unwrap();
        {
            let (writer, sc) = open_writer(&temp, "q1");
            sc.begin().await.unwrap();
            writer.stage_checkpoint("source-a", 777).await.unwrap();
            sc.commit().await.unwrap();
            writer.write_config_hash(0xABCD).await.unwrap();
        }
        // Drop the writer + session above; reopen the DB and create a fresh writer.
        let (writer, _sc) = open_writer(&temp, "q1");
        assert_eq!(writer.read_checkpoint("source-a").await.unwrap(), Some(777));
        assert_eq!(writer.read_config_hash().await.unwrap(), Some(0xABCD));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_session_commits_atomically() {
        let temp = TempDir::new().unwrap();
        let (writer, sc) = open_writer(&temp, "q1");

        // Outer begin (lib's wrap), inner begin (core's process_source_change)
        sc.begin().await.unwrap();
        sc.begin().await.unwrap();
        // Core would do its index writes here; we simulate just the checkpoint stage.
        sc.commit().await.unwrap(); // inner no-op
        writer.stage_checkpoint("source-a", 555).await.unwrap();
        sc.commit().await.unwrap(); // outer real commit

        assert_eq!(writer.read_checkpoint("source-a").await.unwrap(), Some(555));
    }
}
