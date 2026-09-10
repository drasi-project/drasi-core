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

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::{CheckpointStore, IndexError, SourceCheckpoint};
use rocksdb::{Direction, IteratorMode, WriteBatchWithTransaction};

use crate::{checkpoint::STREAM_STATE_CF, IndexDb, RocksDbSessionState};

use drasi_core::computation::ComputationIoScope as BlockingScope;

const SOURCE_SEQUENCE: &str = "source_sequence:";
const SOURCE_POSITION: &str = "source_position:";

fn number(bytes: Vec<u8>) -> Result<u64, IndexError> {
    Ok(u64::from_be_bytes(
        bytes.try_into().map_err(|_| IndexError::CorruptedData)?,
    ))
}

fn checkpoint(
    sequence: Option<Vec<u8>>,
    position: impl FnOnce() -> Result<Option<Vec<u8>>, IndexError>,
) -> Result<Option<SourceCheckpoint>, IndexError> {
    sequence
        .map(|bytes| {
            Ok(SourceCheckpoint {
                sequence: number(bytes)?,
                source_position: position()?.map(Bytes::from),
            })
        })
        .transpose()
}

fn checkpoints(
    entries: impl Iterator<Item = Result<(Box<[u8]>, Box<[u8]>), rocksdb::Error>>,
    get_position: impl Fn(&str) -> Result<Option<Vec<u8>>, IndexError>,
) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
    let mut result = HashMap::new();
    for entry in entries {
        let (key, value) = entry.map_err(IndexError::other)?;
        if !key.starts_with(SOURCE_SEQUENCE.as_bytes()) {
            break;
        }
        let source =
            std::str::from_utf8(&key[SOURCE_SEQUENCE.len()..]).map_err(IndexError::other)?;
        result.insert(
            source.to_owned(),
            SourceCheckpoint {
                sequence: number(value.to_vec())?,
                source_position: get_position(source)?.map(Bytes::from),
            },
        );
    }
    Ok(result)
}

pub(super) struct ComputationCheckpointStore {
    db: Arc<IndexDb>,
    session: Arc<RocksDbSessionState>,
    work: Arc<BlockingScope>,
}

impl ComputationCheckpointStore {
    pub(super) fn new(
        db: Arc<IndexDb>,
        session: Arc<RocksDbSessionState>,
        work: Arc<BlockingScope>,
    ) -> Self {
        Self { db, session, work }
    }

    async fn read_number(&self, key: String) -> Result<Option<u64>, IndexError> {
        let db = self.db.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                db.get_cf(&cf, key)
                    .map_err(IndexError::other)?
                    .map(number)
                    .transpose()
            })
            .await
    }
}

#[async_trait]
impl CheckpointStore for ComputationCheckpointStore {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let source_id = source_id.to_owned();
        let position = source_position.cloned();
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                session.with_txn(|transaction| {
                    transaction
                        .put_cf(
                            &cf,
                            format!("{SOURCE_SEQUENCE}{source_id}"),
                            sequence.to_be_bytes(),
                        )
                        .map_err(IndexError::other)?;
                    let key = format!("{SOURCE_POSITION}{source_id}");
                    match position {
                        Some(position) => transaction.put_cf(&cf, key, position),
                        None => transaction.delete_cf(&cf, key),
                    }
                    .map_err(IndexError::other)
                })
            })
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        let sequence = format!("{SOURCE_SEQUENCE}{source_id}");
        let position = format!("{SOURCE_POSITION}{source_id}");
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                session.with_txn_or_db(
                    |transaction| {
                        checkpoint(
                            transaction
                                .get_cf(&cf, &sequence)
                                .map_err(IndexError::other)?,
                            || {
                                transaction
                                    .get_cf(&cf, &position)
                                    .map_err(IndexError::other)
                            },
                        )
                    },
                    |db| {
                        checkpoint(
                            db.get_cf(&cf, &sequence).map_err(IndexError::other)?,
                            || db.get_cf(&cf, &position).map_err(IndexError::other),
                        )
                    },
                )
            })
            .await
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                session.with_txn_or_db(
                    |transaction| {
                        checkpoints(
                            transaction.iterator_cf(
                                &cf,
                                IteratorMode::From(SOURCE_SEQUENCE.as_bytes(), Direction::Forward),
                            ),
                            |source| {
                                transaction
                                    .get_cf(&cf, format!("{SOURCE_POSITION}{source}"))
                                    .map_err(IndexError::other)
                            },
                        )
                    },
                    |db| {
                        checkpoints(
                            db.iterator_cf(
                                &cf,
                                IteratorMode::From(SOURCE_SEQUENCE.as_bytes(), Direction::Forward),
                            ),
                            |source| {
                                db.get_cf(&cf, format!("{SOURCE_POSITION}{source}"))
                                    .map_err(IndexError::other)
                            },
                        )
                    },
                )
            })
            .await
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let db = self.db.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                let mut batch = WriteBatchWithTransaction::<true>::default();
                for entry in db.iterator_cf(&cf, IteratorMode::Start) {
                    let (key, _) = entry.map_err(IndexError::other)?;
                    batch.delete_cf(&cf, key);
                }
                db.write(batch).map_err(IndexError::other)
            })
            .await
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let db = self.db.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                db.put_cf(&cf, b"config_hash", hash.to_be_bytes())
                    .map_err(IndexError::other)
            })
            .await
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        self.read_number("config_hash".to_owned()).await
    }

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        let key = format!("result_sequence:{query_id}");
        let db = self.db.clone();
        let session = self.session.clone();
        self.work
            .run(move || {
                let cf = db
                    .cf_handle(STREAM_STATE_CF)
                    .ok_or(IndexError::CorruptedData)?;
                session.with_txn(|transaction| {
                    transaction
                        .put_cf(&cf, key, sequence.to_be_bytes())
                        .map_err(IndexError::other)
                })
            })
            .await
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.read_number(format!("result_sequence:{query_id}"))
            .await
    }
}
