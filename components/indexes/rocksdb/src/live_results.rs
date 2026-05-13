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

//! RocksDB implementation of [`LiveResultsWriter`].
//!
//! Uses a dedicated `live_results` column family with keys formatted as:
//! `{query_id}\x00{row_signature_u64_be}` (8-byte big-endian suffix).
//!
//! Writes are standalone (not part of the session transaction) since live results
//! persistence happens after the index transaction commits.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{IndexError, LiveResultsWriter, RowMutation};
use rocksdb::{ColumnFamilyDescriptor, IteratorMode, OptimisticTransactionDB, Options, WriteBatchWithTransaction};
use tokio::task;

/// Column family name for live results data.
pub(crate) const LIVE_RESULTS_CF: &str = "live_results";

/// Returns the column family descriptor for the live_results CF.
pub(crate) fn live_results_cf_descriptor() -> ColumnFamilyDescriptor {
    let opts = Options::default();
    ColumnFamilyDescriptor::new(LIVE_RESULTS_CF, opts)
}

/// Build the live results key: `{query_id}\x00{row_signature_be_bytes}`.
fn make_key(query_id: &str, row_signature: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(query_id.len() + 1 + 8);
    key.extend_from_slice(query_id.as_bytes());
    key.push(0x00);
    key.extend_from_slice(&row_signature.to_be_bytes());
    key
}

/// Extract the row signature from a live results key.
fn signature_from_key(key: &[u8], prefix_len: usize) -> Option<u64> {
    if key.len() < prefix_len + 8 {
        return None;
    }
    let sig_bytes: [u8; 8] = key[prefix_len..prefix_len + 8].try_into().ok()?;
    Some(u64::from_be_bytes(sig_bytes))
}

/// Build the prefix for scanning all entries of a query: `{query_id}\x00`.
fn make_prefix(query_id: &str) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(query_id.len() + 1);
    prefix.extend_from_slice(query_id.as_bytes());
    prefix.push(0x00);
    prefix
}

/// RocksDB-backed live results writer.
///
/// Stores serialized row data keyed by `(query_id, row_signature)`.
/// Uses WriteBatch for atomic multi-row mutations.
pub struct RocksDbLiveResultsWriter {
    db: Arc<OptimisticTransactionDB>,
}

impl RocksDbLiveResultsWriter {
    pub fn new(db: Arc<OptimisticTransactionDB>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl LiveResultsWriter for RocksDbLiveResultsWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        let db = self.db.clone();
        // Collect owned mutation data for the blocking task
        let owned_mutations: Vec<(Vec<u8>, Option<Vec<u8>>)> = mutations
            .iter()
            .map(|m| {
                let key = make_key(query_id, m.row_signature);
                let data = m.data.map(|d| d.to_vec());
                (key, data)
            })
            .collect();

        task::spawn_blocking(move || {
            let cf = db
                .cf_handle(LIVE_RESULTS_CF)
                .expect("live_results cf not found");

            let mut batch = WriteBatchWithTransaction::<true>::default();
            for (key, data) in &owned_mutations {
                match data {
                    Some(value) => batch.put_cf(&cf, key, value),
                    None => batch.delete_cf(&cf, key),
                }
            }
            db.write(batch).map_err(IndexError::other)
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);

        task::spawn_blocking(move || {
            let cf = db
                .cf_handle(LIVE_RESULTS_CF)
                .expect("live_results cf not found");
            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&prefix, rocksdb::Direction::Forward),
            );

            let mut entries = Vec::new();
            for item in iter {
                match item {
                    Ok((key, value)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        if let Some(sig) = signature_from_key(&key, prefix.len()) {
                            entries.push((sig, value.to_vec()));
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

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);

        task::spawn_blocking(move || {
            let cf = db
                .cf_handle(LIVE_RESULTS_CF)
                .expect("live_results cf not found");
            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&prefix, rocksdb::Direction::Forward),
            );

            let mut batch = WriteBatchWithTransaction::<true>::default();
            for item in iter {
                match item {
                    Ok((key, _)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        batch.delete_cf(&cf, &key);
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            db.write(batch).map_err(IndexError::other)
        })
        .await
        .map_err(IndexError::other)?
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        let db = self.db.clone();
        let prefix = make_prefix(query_id);

        task::spawn_blocking(move || {
            let cf = db
                .cf_handle(LIVE_RESULTS_CF)
                .expect("live_results cf not found");
            let iter = db.iterator_cf(
                &cf,
                IteratorMode::From(&prefix, rocksdb::Direction::Forward),
            );

            let mut count = 0;
            for item in iter {
                match item {
                    Ok((key, _)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        count += 1;
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            Ok(count)
        })
        .await
        .map_err(IndexError::other)?
    }
}
