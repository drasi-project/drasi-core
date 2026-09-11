// Copyright 2024 The Drasi Authors.
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

//! RocksDB implementation of [`CheckpointStore`].
//!
//! Uses a dedicated `stream_state` column family to store per-source checkpoint
//! sequences, opaque source position bytes, and a config hash.
//!
//! Keys:
//! - `source_sequence:{source_id}` → 8-byte big-endian `u64` sequence
//! - `source_position:{source_id}` → raw opaque bytes
//! - `config_hash` → 8-byte big-endian `u64`
//!
//! `stage_checkpoint` writes into the active session transaction so it commits
//! atomically with index updates. Clears, config hashes and result sequences
//! use the active transaction when present, including read-your-writes.

use std::collections::HashMap;
use std::sync::Arc;

use crate::IndexDb;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::{CheckpointStore, IndexError, SourceCheckpoint};
use rocksdb::ColumnFamilyDescriptor;
use tokio::task;

use crate::RocksDbSessionState;

/// Column family name for checkpoint/stream state data.
pub(crate) const STREAM_STATE_CF: &str = "stream_state";

const SOURCE_SEQUENCE_PREFIX: &str = "source_sequence:";
const SOURCE_POSITION_PREFIX: &str = "source_position:";
const CONFIG_HASH_KEY: &str = "config_hash";
const RESULT_SEQUENCE_PREFIX: &str = "result_sequence:";

/// Returns the column family descriptor for the stream_state CF.
pub(crate) fn stream_state_cf_descriptor(
    options: &crate::RocksIndexOptions,
) -> ColumnFamilyDescriptor {
    let block_cache = options.memory_budget().block_cache();
    let mut opts = crate::cf_options::base_cf_options(block_cache);
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(16));
    crate::sizing::descriptor(STREAM_STATE_CF, opts, options)
}

/// RocksDB-backed checkpoint store.
///
/// Shares a `RocksDbSessionState` with the result/element/future indexes so that
/// `stage_checkpoint` writes land in the same transaction as index updates.
pub struct RocksDbCheckpointStore {
    db: Arc<IndexDb>,
    session_state: Arc<RocksDbSessionState>,
}

impl RocksDbCheckpointStore {
    pub fn new(db: Arc<IndexDb>, session_state: Arc<RocksDbSessionState>) -> Self {
        Self { db, session_state }
    }

    async fn write_u64(&self, key: Vec<u8>, value: u64) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session = self.session_state.operation()?;
        task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .ok_or(IndexError::CorruptedData)?;
            session.with_txn_or_db(
                |txn| {
                    txn.put_cf(&cf, &key, value.to_be_bytes())
                        .map_err(IndexError::other)
                },
                |db| {
                    db.put_cf(&cf, &key, value.to_be_bytes())
                        .map_err(IndexError::other)
                },
            )
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn read_u64(&self, key: Vec<u8>) -> Result<Option<u64>, IndexError> {
        let db = self.db.clone();
        let session = self.session_state.operation()?;
        task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .ok_or(IndexError::CorruptedData)?;
            let data = session.with_txn_or_db(
                |txn| txn.get_cf(&cf, &key).map_err(IndexError::other),
                |db| db.get_cf(&cf, &key).map_err(IndexError::other),
            )?;
            data.map(|value| {
                let bytes: [u8; 8] = value.try_into().map_err(|_| IndexError::CorruptedData)?;
                Ok(u64::from_be_bytes(bytes))
            })
            .transpose()
        })
        .await
        .map_err(IndexError::other)?
    }
}

/// Shared helper: given the result of reading the sequence key, parse it and
/// look up the corresponding position key using the supplied `get_pos` closure.
/// Works with both `Transaction::get_cf` and `DB::get_cf`.
fn read_checkpoint_from_seq_data(
    seq_data: Option<Vec<u8>>,
    source_id: &str,
    get_pos: impl FnOnce(&str) -> Result<Option<Vec<u8>>, IndexError>,
) -> Result<Option<SourceCheckpoint>, IndexError> {
    match seq_data {
        Some(v) => {
            let seq_bytes: [u8; 8] = v.try_into().map_err(|_| IndexError::CorruptedData)?;
            let sequence = u64::from_be_bytes(seq_bytes);
            let pos_key = format!("{SOURCE_POSITION_PREFIX}{source_id}");
            let pos_data = get_pos(&pos_key)?;
            let source_position = pos_data.map(Bytes::from);
            Ok(Some(SourceCheckpoint {
                sequence,
                source_position,
            }))
        }
        None => Ok(None),
    }
}

/// Shared helper for `read_all_checkpoints` that works with both
/// `Transaction::prefix_iterator_cf` and `DB::prefix_iterator_cf`.
fn read_all_checkpoints_impl(
    _cf: &impl rocksdb::AsColumnFamilyRef,
    iter: impl Iterator<Item = Result<(Box<[u8]>, Box<[u8]>), rocksdb::Error>>,
    get_pos: impl Fn(&str) -> Result<Option<Vec<u8>>, IndexError>,
) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
    let mut result: HashMap<String, SourceCheckpoint> = HashMap::new();
    let prefix = SOURCE_SEQUENCE_PREFIX.as_bytes();

    for item in iter {
        match item {
            Ok((key, value)) => {
                if !key.starts_with(prefix) {
                    break;
                }
                let source_id = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                let seq_bytes: [u8; 8] = match value.as_ref().try_into() {
                    Ok(v) => v,
                    Err(_) => return Err(IndexError::CorruptedData),
                };
                let sequence = u64::from_be_bytes(seq_bytes);

                let pos_key = format!("{SOURCE_POSITION_PREFIX}{source_id}");
                let pos_data = get_pos(&pos_key)?;
                let source_position = pos_data.map(Bytes::from);

                result.insert(
                    source_id,
                    SourceCheckpoint {
                        sequence,
                        source_position,
                    },
                );
            }
            Err(e) => return Err(IndexError::other(e)),
        }
    }

    Ok(result)
}

#[async_trait]
impl CheckpointStore for RocksDbCheckpointStore {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.operation()?;
        let source_id = source_id.to_string();
        let source_position_owned = source_position.map(|b| b.to_vec());

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            session_state.with_txn(|txn| {
                let seq_key = format!("{SOURCE_SEQUENCE_PREFIX}{source_id}");
                txn.put_cf(&cf, &seq_key, sequence.to_be_bytes())
                    .map_err(IndexError::other)?;

                let pos_key = format!("{SOURCE_POSITION_PREFIX}{source_id}");
                match &source_position_owned {
                    Some(pos) => {
                        txn.put_cf(&cf, &pos_key, pos).map_err(IndexError::other)?;
                    }
                    None => {
                        txn.delete_cf(&cf, &pos_key).map_err(IndexError::other)?;
                    }
                }

                Ok(())
            })
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.operation()?;
        let source_id = source_id.to_string();

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            // Read from the active transaction if one exists (to see staged
            // but uncommitted writes), otherwise read committed data from the
            // DB directly (startup path, before any session is opened).
            session_state.with_txn_or_db(
                |txn| {
                    let seq_key = format!("{SOURCE_SEQUENCE_PREFIX}{source_id}");
                    let seq_data = txn.get_cf(&cf, &seq_key).map_err(IndexError::other)?;
                    read_checkpoint_from_seq_data(seq_data, &source_id, |pos_key| {
                        txn.get_cf(&cf, pos_key).map_err(IndexError::other)
                    })
                },
                |db| {
                    let seq_key = format!("{SOURCE_SEQUENCE_PREFIX}{source_id}");
                    let seq_data = db.get_cf(&cf, &seq_key).map_err(IndexError::other)?;
                    read_checkpoint_from_seq_data(seq_data, &source_id, |pos_key| {
                        db.get_cf(&cf, pos_key).map_err(IndexError::other)
                    })
                },
            )
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.operation()?;

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            // Use transaction if active (sees staged writes), else read from DB directly.
            let prefix = SOURCE_SEQUENCE_PREFIX.as_bytes();
            session_state.with_txn_or_db(
                |txn| {
                    let iter = txn.prefix_iterator_cf(&cf, prefix);
                    read_all_checkpoints_impl(&cf, iter, |k| {
                        txn.get_cf(&cf, k).map_err(IndexError::other)
                    })
                },
                |db| {
                    let iter = db.prefix_iterator_cf(&cf, prefix);
                    read_all_checkpoints_impl(&cf, iter, |k| {
                        db.get_cf(&cf, k).map_err(IndexError::other)
                    })
                },
            )
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let session = self.session_state.operation()?;
        task::spawn_blocking(move || {
            session.clear_ranges(
                &[
                    (STREAM_STATE_CF, SOURCE_SEQUENCE_PREFIX.as_bytes()),
                    (STREAM_STATE_CF, SOURCE_POSITION_PREFIX.as_bytes()),
                ],
                &[(STREAM_STATE_CF, CONFIG_HASH_KEY.as_bytes())],
            )
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        self.write_u64(CONFIG_HASH_KEY.as_bytes().to_vec(), hash)
            .await
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        self.read_u64(CONFIG_HASH_KEY.as_bytes().to_vec()).await
    }

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        self.write_u64(
            format!("{RESULT_SEQUENCE_PREFIX}{query_id}").into_bytes(),
            sequence,
        )
        .await
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.read_u64(format!("{RESULT_SEQUENCE_PREFIX}{query_id}").into_bytes())
            .await
    }
}
