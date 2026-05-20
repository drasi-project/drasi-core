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

//! RocksDB implementation of [`OutboxWriter`].
//!
//! Uses a dedicated `outbox` column family with keys formatted as:
//! `{query_id}:{sequence_u64_be}` (8-byte big-endian suffix for ordered iteration).
//!
//! Writes are standalone (not part of the session transaction) since outbox
//! persistence happens after the index transaction commits.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{IndexError, OutboxWriter};
use rocksdb::{ColumnFamilyDescriptor, IteratorMode, OptimisticTransactionDB, Options};
use tokio::task;

/// Column family name for outbox data.
pub(crate) const OUTBOX_CF: &str = "outbox";

/// Returns the column family descriptor for the outbox CF.
pub(crate) fn outbox_cf_descriptor() -> ColumnFamilyDescriptor {
    let opts = Options::default();
    ColumnFamilyDescriptor::new(OUTBOX_CF, opts)
}

/// Build the outbox key: `{query_id}\x00{sequence_be_bytes}`.
///
/// The null separator ensures correct prefix scanning even with variable-length query IDs.
fn make_key(query_id: &str, sequence: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(query_id.len() + 1 + 8);
    key.extend_from_slice(query_id.as_bytes());
    key.push(0x00);
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

/// Extract the sequence number from an outbox key.
fn sequence_from_key(key: &[u8], prefix_len: usize) -> Option<u64> {
    if key.len() < prefix_len + 8 {
        return None;
    }
    let seq_bytes: [u8; 8] = key[prefix_len..prefix_len + 8].try_into().ok()?;
    Some(u64::from_be_bytes(seq_bytes))
}

/// Build the prefix for scanning all entries of a query: `{query_id}\x00`.
fn make_prefix(query_id: &str) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(query_id.len() + 1);
    prefix.extend_from_slice(query_id.as_bytes());
    prefix.push(0x00);
    prefix
}

/// RocksDB-backed outbox writer.
///
/// Stores serialized query results in a column family with ordered keys
/// for efficient range reads and trimming.
pub struct RocksDbOutboxWriter {
    db: Arc<OptimisticTransactionDB>,
}

impl RocksDbOutboxWriter {
    pub fn new(db: Arc<OptimisticTransactionDB>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl OutboxWriter for RocksDbOutboxWriter {
    async fn append(
        &self,
        query_id: &str,
        sequence: u64,
        data: &[u8],
    ) -> Result<(), IndexError> {
        let db = self.db.clone();
        let key = make_key(query_id, sequence);
        let data = data.to_vec();

        task::spawn_blocking(move || {
            let cf = db.cf_handle(OUTBOX_CF).expect("outbox cf not found");
            db.put_cf(&cf, &key, &data).map_err(IndexError::other)
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);
        let start_key = make_key(query_id, after_sequence.saturating_add(1));

        task::spawn_blocking(move || {
            let cf = db.cf_handle(OUTBOX_CF).expect("outbox cf not found");
            let iter = db.iterator_cf(&cf, IteratorMode::From(&start_key, rocksdb::Direction::Forward));

            let mut entries = Vec::new();
            for item in iter {
                match item {
                    Ok((key, value)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        if let Some(seq) = sequence_from_key(&key, prefix.len()) {
                            entries.push((seq, value.to_vec()));
                        }
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            Ok(entries)
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);
        // Seek to the end of this prefix range by appending the maximum possible
        // sequence suffix (8 bytes of 0xFF). This ensures we land within the
        // exact prefix even when other query IDs share the same prefix string
        // (e.g., "q1" vs "q10").
        let mut end_seek = prefix.clone();
        end_seek.extend_from_slice(&[0xFF; 8]);

        task::spawn_blocking(move || {
            let cf = db.cf_handle(OUTBOX_CF).expect("outbox cf not found");
            // Use reverse iterator from end of prefix range
            let mut iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&end_seek, rocksdb::Direction::Reverse),
            );

            // Only need the first item (highest sequence in this prefix)
            if let Some(item) = iter.next() {
                match item {
                    Ok((key, _)) => {
                        if key.starts_with(&prefix) {
                            return Ok(sequence_from_key(&key, prefix.len()));
                        }
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            Ok(None)
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);

        task::spawn_blocking(move || {
            let cf = db.cf_handle(OUTBOX_CF).expect("outbox cf not found");
            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&prefix, rocksdb::Direction::Forward),
            );

            for item in iter {
                match item {
                    Ok((key, _)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        db.delete_cf(&cf, &key).map_err(IndexError::other)?;
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            Ok(())
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn trim_to_capacity(
        &self,
        query_id: &str,
        capacity: usize,
    ) -> Result<usize, IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);

        task::spawn_blocking(move || {
            let cf = db.cf_handle(OUTBOX_CF).expect("outbox cf not found");

            // First, count total entries
            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&prefix, rocksdb::Direction::Forward),
            );
            let mut keys: Vec<Vec<u8>> = Vec::new();
            for item in iter {
                match item {
                    Ok((key, _)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        keys.push(key.to_vec());
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }

            let total = keys.len();
            if total <= capacity {
                return Ok(0);
            }

            let to_remove = total - capacity;
            for key in keys.iter().take(to_remove) {
                db.delete_cf(&cf, key).map_err(IndexError::other)?;
            }
            Ok(to_remove)
        })
        .await
        .map_err(IndexError::other)?
    }
}
