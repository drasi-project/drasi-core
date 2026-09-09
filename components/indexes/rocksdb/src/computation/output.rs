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

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{IndexError, LiveResultsWriter, OutboxWriter, RowMutation};
use rocksdb::{Direction, IteratorMode};

use crate::{live_results::LIVE_RESULTS_CF, outbox::OUTBOX_CF, IndexDb, RocksDbSessionState};

use super::blocking::BlockingScope;

fn prefix(query_id: &str) -> Result<Vec<u8>, IndexError> {
    if query_id.is_empty() || query_id.contains('\0') {
        return Err(IndexError::other(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "computation output identity must be nonempty and contain no NUL",
        )));
    }
    let mut result = query_id.as_bytes().to_vec();
    result.push(0);
    Ok(result)
}

fn key(prefix: &[u8], sequence: u64) -> Vec<u8> {
    let mut result = prefix.to_vec();
    result.extend_from_slice(&sequence.to_be_bytes());
    result
}

fn sequence(key: &[u8], prefix_len: usize) -> Result<u64, IndexError> {
    let suffix = key.get(prefix_len..).ok_or(IndexError::CorruptedData)?;
    Ok(u64::from_be_bytes(
        suffix.try_into().map_err(|_| IndexError::CorruptedData)?,
    ))
}

fn rows(
    db: &IndexDb,
    column: &str,
    prefix: &[u8],
    after: Option<u64>,
) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
    let cf = db.cf_handle(column).ok_or(IndexError::CorruptedData)?;
    let start = match after {
        Some(u64::MAX) => return Ok(Vec::new()),
        Some(after) => key(prefix, after + 1),
        None => prefix.to_vec(),
    };
    let mut result = Vec::new();
    for entry in db.iterator_cf(&cf, IteratorMode::From(&start, Direction::Forward)) {
        let (key, value) = entry.map_err(IndexError::other)?;
        if !key.starts_with(prefix) {
            break;
        }
        result.push((sequence(&key, prefix.len())?, value.to_vec()));
    }
    Ok(result)
}

pub(super) struct ComputationOutboxWriter {
    db: Arc<IndexDb>,
    session: Arc<RocksDbSessionState>,
    work: Arc<BlockingScope>,
}

impl ComputationOutboxWriter {
    pub(super) fn new(
        db: Arc<IndexDb>,
        session: Arc<RocksDbSessionState>,
        work: Arc<BlockingScope>,
    ) -> Self {
        Self { db, session, work }
    }
}

#[async_trait]
impl OutboxWriter for ComputationOutboxWriter {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        let key = key(&prefix(query_id)?, sequence);
        let data = data.to_vec();
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db.cf_handle(OUTBOX_CF).ok_or(IndexError::CorruptedData)?;
                session.with_txn(|transaction| {
                    transaction
                        .put_cf(&cf, key, data)
                        .map_err(IndexError::other)
                })
            })
            .await
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let prefix = prefix(query_id)?;
        let db = self.db.clone();
        self.work
            .run(move || rows(&db, OUTBOX_CF, &prefix, Some(after_sequence)))
            .await
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        let prefix = prefix(query_id)?;
        let db = self.db.clone();
        self.work
            .run(move || {
                let cf = db.cf_handle(OUTBOX_CF).ok_or(IndexError::CorruptedData)?;
                let end = key(&prefix, u64::MAX);
                let mut entries = db.iterator_cf(&cf, IteratorMode::From(&end, Direction::Reverse));
                match entries.next().transpose().map_err(IndexError::other)? {
                    Some((key, _)) if key.starts_with(&prefix) => {
                        Ok(Some(sequence(&key, prefix.len())?))
                    }
                    _ => Ok(None),
                }
            })
            .await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.trim_to_capacity(query_id, 0).await?;
        Ok(())
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        let prefix = prefix(query_id)?;
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db.cf_handle(OUTBOX_CF).ok_or(IndexError::CorruptedData)?;
                session.with_txn(|transaction| {
                    let mut keys = Vec::new();
                    for entry in transaction
                        .iterator_cf(&cf, IteratorMode::From(&prefix, Direction::Forward))
                    {
                        let (key, _) = entry.map_err(IndexError::other)?;
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        keys.push(key);
                    }
                    let remove = keys.len().saturating_sub(capacity);
                    for key in keys.into_iter().take(remove) {
                        transaction.delete_cf(&cf, key).map_err(IndexError::other)?;
                    }
                    Ok(remove)
                })
            })
            .await
    }
}

pub(super) struct ComputationLiveResultsWriter {
    db: Arc<IndexDb>,
    session: Arc<RocksDbSessionState>,
    work: Arc<BlockingScope>,
}

impl ComputationLiveResultsWriter {
    pub(super) fn new(
        db: Arc<IndexDb>,
        session: Arc<RocksDbSessionState>,
        work: Arc<BlockingScope>,
    ) -> Self {
        Self { db, session, work }
    }
}

#[async_trait]
impl LiveResultsWriter for ComputationLiveResultsWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        let prefix = prefix(query_id)?;
        let mutations: Vec<_> = mutations
            .iter()
            .map(|mutation| {
                (
                    key(&prefix, mutation.row_signature),
                    mutation.data.map(<[u8]>::to_vec),
                )
            })
            .collect();
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(LIVE_RESULTS_CF)
                    .ok_or(IndexError::CorruptedData)?;
                session.with_txn(|transaction| {
                    for (key, data) in mutations {
                        match data {
                            Some(data) => transaction
                                .put_cf(&cf, key, data)
                                .map_err(IndexError::other)?,
                            None => transaction.delete_cf(&cf, key).map_err(IndexError::other)?,
                        }
                    }
                    Ok(())
                })
            })
            .await
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let prefix = prefix(query_id)?;
        let db = self.db.clone();
        self.work
            .run(move || rows(&db, LIVE_RESULTS_CF, &prefix, None))
            .await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let prefix = prefix(query_id)?;
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(LIVE_RESULTS_CF)
                    .ok_or(IndexError::CorruptedData)?;
                session.with_txn(|transaction| {
                    for entry in transaction
                        .iterator_cf(&cf, IteratorMode::From(&prefix, Direction::Forward))
                    {
                        let (key, _) = entry.map_err(IndexError::other)?;
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        transaction.delete_cf(&cf, key).map_err(IndexError::other)?;
                    }
                    Ok(())
                })
            })
            .await
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        let prefix = prefix(query_id)?;
        let db = self.db.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(LIVE_RESULTS_CF)
                    .ok_or(IndexError::CorruptedData)?;
                let mut count = 0;
                for entry in db.iterator_cf(&cf, IteratorMode::From(&prefix, Direction::Forward)) {
                    let (key, _) = entry.map_err(IndexError::other)?;
                    if !key.starts_with(&prefix) {
                        break;
                    }
                    count += 1;
                }
                Ok(count)
            })
            .await
    }
}
