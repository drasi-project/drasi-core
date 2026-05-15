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
//! atomically with index updates.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::{CheckpointStore, IndexError, SourceCheckpoint};
use rocksdb::{ColumnFamilyDescriptor, OptimisticTransactionDB, Options};
use tokio::task;

use crate::RocksDbSessionState;

/// Column family name for checkpoint/stream state data.
pub(crate) const STREAM_STATE_CF: &str = "stream_state";

const SOURCE_SEQUENCE_PREFIX: &str = "source_sequence:";
const SOURCE_POSITION_PREFIX: &str = "source_position:";
const CONFIG_HASH_KEY: &str = "config_hash";

/// Returns the column family descriptor for the stream_state CF.
pub(crate) fn stream_state_cf_descriptor() -> ColumnFamilyDescriptor {
    let mut opts = Options::default();
    opts.set_prefix_extractor(rocksdb::SliceTransform::create_fixed_prefix(16));
    ColumnFamilyDescriptor::new(STREAM_STATE_CF, opts)
}

/// RocksDB-backed checkpoint store.
///
/// Shares a `RocksDbSessionState` with the result/element/future indexes so that
/// `stage_checkpoint` writes land in the same transaction as index updates.
pub struct RocksDbCheckpointStore {
    db: Arc<OptimisticTransactionDB>,
    session_state: Arc<RocksDbSessionState>,
}

impl RocksDbCheckpointStore {
    pub fn new(db: Arc<OptimisticTransactionDB>, session_state: Arc<RocksDbSessionState>) -> Self {
        Self { db, session_state }
    }
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
        let session_state = self.session_state.clone();
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
        let session_state = self.session_state.clone();
        let source_id = source_id.to_string();

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            session_state.with_txn(|txn| {
                let seq_key = format!("{SOURCE_SEQUENCE_PREFIX}{source_id}");
                let seq_data = txn.get_cf(&cf, &seq_key).map_err(IndexError::other)?;

                match seq_data {
                    Some(v) => {
                        let seq_bytes: [u8; 8] =
                            v.try_into().map_err(|_| IndexError::CorruptedData)?;
                        let sequence = u64::from_be_bytes(seq_bytes);

                        let pos_key = format!("{SOURCE_POSITION_PREFIX}{source_id}");
                        let pos_data = txn.get_cf(&cf, &pos_key).map_err(IndexError::other)?;
                        let source_position = pos_data.map(Bytes::from);

                        Ok(Some(SourceCheckpoint {
                            sequence,
                            source_position,
                        }))
                    }
                    None => Ok(None),
                }
            })
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            session_state.with_txn(|txn| {
                let mut result: HashMap<String, SourceCheckpoint> = HashMap::new();

                // Scan source_sequence: prefix to find all source IDs with checkpoints
                let prefix = SOURCE_SEQUENCE_PREFIX.as_bytes();
                let iter = txn.prefix_iterator_cf(&cf, prefix);
                for item in iter {
                    match item {
                        Ok((key, value)) => {
                            if !key.starts_with(prefix) {
                                break;
                            }
                            let source_id =
                                String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                            let seq_bytes: [u8; 8] = match value.as_ref().try_into() {
                                Ok(v) => v,
                                Err(_) => return Err(IndexError::CorruptedData),
                            };
                            let sequence = u64::from_be_bytes(seq_bytes);

                            // Read corresponding position
                            let pos_key = format!("{SOURCE_POSITION_PREFIX}{source_id}");
                            let pos_data = txn.get_cf(&cf, &pos_key).map_err(IndexError::other)?;
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
            })
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            session_state.with_txn(|txn| {
                // Delete all source_sequence: keys
                let seq_prefix = SOURCE_SEQUENCE_PREFIX.as_bytes();
                let keys_to_delete: Vec<Vec<u8>> = txn
                    .prefix_iterator_cf(&cf, seq_prefix)
                    .take_while(|item| {
                        item.as_ref()
                            .map(|(k, _)| k.starts_with(seq_prefix))
                            .unwrap_or(false)
                    })
                    .filter_map(|item| item.ok().map(|(k, _)| k.to_vec()))
                    .collect();

                for key in &keys_to_delete {
                    txn.delete_cf(&cf, key).map_err(IndexError::other)?;
                }

                // Delete all source_position: keys
                let pos_prefix = SOURCE_POSITION_PREFIX.as_bytes();
                let keys_to_delete: Vec<Vec<u8>> = txn
                    .prefix_iterator_cf(&cf, pos_prefix)
                    .take_while(|item| {
                        item.as_ref()
                            .map(|(k, _)| k.starts_with(pos_prefix))
                            .unwrap_or(false)
                    })
                    .filter_map(|item| item.ok().map(|(k, _)| k.to_vec()))
                    .collect();

                for key in &keys_to_delete {
                    txn.delete_cf(&cf, key).map_err(IndexError::other)?;
                }

                // Delete config hash
                txn.delete_cf(&cf, CONFIG_HASH_KEY)
                    .map_err(IndexError::other)?;

                Ok(())
            })
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            session_state.with_txn(|txn| {
                txn.put_cf(&cf, CONFIG_HASH_KEY, hash.to_be_bytes())
                    .map_err(IndexError::other)?;
                Ok(())
            })
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();

        let task = task::spawn_blocking(move || {
            let cf = db
                .cf_handle(STREAM_STATE_CF)
                .expect("stream_state cf not found");

            session_state.with_txn(|txn| {
                let data = txn
                    .get_cf(&cf, CONFIG_HASH_KEY)
                    .map_err(IndexError::other)?;
                match data {
                    Some(v) => {
                        let bytes: [u8; 8] = v.try_into().map_err(|_| IndexError::CorruptedData)?;
                        Ok(Some(u64::from_be_bytes(bytes)))
                    }
                    None => Ok(None),
                }
            })
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }
}
