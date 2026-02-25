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

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, IndexError, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter,
    },
};
use hashers::jenkins::spooky_hash::SpookyHasher;
use ordered_float::OrderedFloat;
use prost::{
    bytes::{Bytes, BytesMut},
    Message,
};
use rocksdb::{
    Direction, IteratorMode, MergeOperands, OptimisticTransactionDB, Options, SliceTransform,
};
use tokio::task;

use crate::storage_models::{StoredValueAccumulator, StoredValueAccumulatorContainer};
use crate::RocksDbSessionState;

/// RocksDB accumulator index store
///
/// Column Families
///     - values [set_id] -> ValueAccumulator
///     - sorted-sets [set_id + value] -> count {8 byte prefix (set_id)}
pub struct RocksDbResultIndex {
    db: Arc<OptimisticTransactionDB>,
    session_state: Arc<RocksDbSessionState>,
}

const VALUES_CF: &str = "values";
const SETS_CF: &str = "sorted-sets";
const METADATA_CF: &str = "metadata";
const VALUES_BLOCK_CACHE_SIZE: u64 = 32;

impl RocksDbResultIndex {
    /// Create a new RocksDbResultIndex from a shared database handle.
    ///
    /// The database must already have the required column families created.
    /// Use `open_unified_db()` to open a database with all required CFs.
    pub fn new(db: Arc<OptimisticTransactionDB>, session_state: Arc<RocksDbSessionState>) -> Self {
        RocksDbResultIndex { db, session_state }
    }
}

#[async_trait]
impl AccumulatorIndex for RocksDbResultIndex {
    #[tracing::instrument(name = "ari::get", skip_all, err)]
    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();
        let set_id = get_hash_key(owner, key);

        let task = task::spawn_blocking(move || {
            let values_cf = db.cf_handle(VALUES_CF).expect("values cf not found");
            let txn_guard = session_state.lock()?;

            let data = match txn_guard.as_ref() {
                Some(txn) => txn.get_cf(&values_cf, set_id.to_be_bytes()),
                None => db.get_cf(&values_cf, set_id.to_be_bytes()),
            };

            match data {
                Ok(Some(v)) => {
                    let value = match StoredValueAccumulatorContainer::decode(v.as_slice()) {
                        Ok(container) => match container.value {
                            Some(value) => value,
                            None => return Err(IndexError::CorruptedData),
                        },
                        Err(e) => return Err(IndexError::other(e)),
                    };
                    let value: ValueAccumulator = value.into();
                    Ok(Some(value))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(name = "ari::set", skip_all, err)]
    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();
        let set_id = get_hash_key(&owner, &key);

        let task = task::spawn_blocking(move || {
            let values_cf = db.cf_handle(VALUES_CF).expect("values cf not found");
            let txn_guard = session_state.lock()?;

            if let Some(txn) = txn_guard.as_ref() {
                match value {
                    None => match txn.delete_cf(&values_cf, set_id.to_be_bytes()) {
                        Ok(_) => Ok(()),
                        Err(e) => Err(IndexError::other(e)),
                    },
                    Some(v) => {
                        let value: StoredValueAccumulator = v.into();
                        let value = value.serialize();
                        match txn.put_cf(&values_cf, set_id.to_be_bytes(), &value) {
                            Ok(_) => Ok(()),
                            Err(e) => Err(IndexError::other(e)),
                        }
                    }
                }
            } else {
                drop(txn_guard);
                match value {
                    None => match db.delete_cf(&values_cf, set_id.to_be_bytes()) {
                        Ok(_) => Ok(()),
                        Err(e) => Err(IndexError::other(e)),
                    },
                    Some(v) => {
                        let value: StoredValueAccumulator = v.into();
                        let value = value.serialize();
                        match db.put_cf(&values_cf, set_id.to_be_bytes(), &value) {
                            Ok(_) => Ok(()),
                            Err(e) => Err(IndexError::other(e)),
                        }
                    }
                }
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let db = self.db.clone();
        let task = task::spawn_blocking(move || {
            if let Err(err) = db.drop_cf(VALUES_CF) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = db.create_cf(VALUES_CF, &get_value_cf_options()) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = db.drop_cf(SETS_CF) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = db.create_cf(SETS_CF, &get_lss_cf_options()) {
                return Err(IndexError::other(err));
            }
            Ok(())
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }
}

#[async_trait]
impl LazySortedSetStore for RocksDbResultIndex {
    #[tracing::instrument(name = "lss::get_next", skip_all, err)]
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();

        let task = task::spawn_blocking(move || {
            let set_cf = db.cf_handle(SETS_CF).expect("sorted-sets cf not found");
            let prefix = set_id.to_be_bytes();
            let txn_guard = session_state.lock()?;

            match value {
                Some(value) => {
                    let from_key = encode_next_set_value_key(set_id, value);
                    let iter: Box<
                        dyn Iterator<Item = Result<(Box<[u8]>, Box<[u8]>), rocksdb::Error>>,
                    > = match txn_guard.as_ref() {
                        Some(txn) => Box::new(txn.iterator_cf(
                            &set_cf,
                            IteratorMode::From(&from_key, Direction::Forward),
                        )),
                        None => Box::new(db.iterator_cf(
                            &set_cf,
                            IteratorMode::From(&from_key, Direction::Forward),
                        )),
                    };
                    for item in iter {
                        match item {
                            Ok((key, count)) => {
                                if !key.starts_with(&prefix) {
                                    break;
                                }
                                let value = decode_set_value_key(&key)?;
                                let count = decode_count(&count)?;
                                if count == 0 {
                                    continue;
                                }

                                return Ok(Some((value, count)));
                            }
                            Err(e) => return Err(IndexError::other(e)),
                        }
                    }
                }
                None => {
                    let iter: Box<
                        dyn Iterator<Item = Result<(Box<[u8]>, Box<[u8]>), rocksdb::Error>>,
                    > = match txn_guard.as_ref() {
                        Some(txn) => Box::new(txn.prefix_iterator_cf(&set_cf, prefix)),
                        None => Box::new(db.prefix_iterator_cf(&set_cf, prefix)),
                    };
                    for item in iter {
                        match item {
                            Ok((key, count)) => {
                                if !key.starts_with(&prefix) {
                                    break;
                                }
                                let value = decode_set_value_key(&key)?;
                                let count = decode_count(&count)?;
                                if count == 0 {
                                    continue;
                                }

                                return Ok(Some((value, count)));
                            }
                            Err(e) => return Err(IndexError::other(e)),
                        }
                    }
                }
            };
            Ok(None)
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(name = "lss::get_value_count", skip_all, err)]
    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();
        let task = task::spawn_blocking(move || {
            let set_cf = db.cf_handle(SETS_CF).expect("sorted-sets cf not found");
            let key = encode_set_value_key(set_id, value);
            let txn_guard = session_state.lock()?;

            let data = match txn_guard.as_ref() {
                Some(txn) => txn.get_cf(&set_cf, key),
                None => db.get_cf(&set_cf, key),
            };

            match data {
                Ok(Some(v)) => decode_count(&v),
                Ok(None) => Ok(0),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(name = "lss::increment_value_count", skip_all, err)]
    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();

        let task = task::spawn_blocking(move || {
            let set_cf = db.cf_handle(SETS_CF).expect("sorted-sets cf not found");
            let key = encode_set_value_key(set_id, value);
            let txn_guard = session_state.lock()?;

            let result = match txn_guard.as_ref() {
                Some(txn) => txn.merge_cf(&set_cf, key, encode_count(delta)),
                None => db.merge_cf(&set_cf, key, encode_count(delta)),
            };

            match result {
                Ok(_) => Ok(()),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }
}

#[async_trait]
impl ResultSequenceCounter for RocksDbResultIndex {
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();
        let source_change_id = source_change_id.to_string();
        let task = task::spawn_blocking(move || {
            let metadata_cf = db.cf_handle(METADATA_CF).expect("metadata cf not found");
            let txn_guard = session_state.lock()?;

            if let Some(txn) = txn_guard.as_ref() {
                if let Err(e) = txn.put_cf(&metadata_cf, "sequence", sequence.to_be_bytes()) {
                    return Err(IndexError::other(e));
                }
                if let Err(e) = txn.put_cf(
                    &metadata_cf,
                    "source_change_id",
                    source_change_id.as_bytes(),
                ) {
                    return Err(IndexError::other(e));
                }
                Ok(())
            } else {
                drop(txn_guard);
                let mut batch = rocksdb::WriteBatchWithTransaction::default();
                batch.put_cf(&metadata_cf, "sequence", sequence.to_be_bytes());
                batch.put_cf(
                    &metadata_cf,
                    "source_change_id",
                    source_change_id.as_bytes(),
                );
                match db.write(batch) {
                    Ok(_) => Ok(()),
                    Err(e) => Err(IndexError::other(e)),
                }
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let db = self.db.clone();
        let session_state = self.session_state.clone();
        let task = task::spawn_blocking(move || {
            let metadata_cf = db.cf_handle(METADATA_CF).expect("metadata cf not found");
            let txn_guard = session_state.lock()?;

            let seq_data = match txn_guard.as_ref() {
                Some(txn) => txn.get_cf(&metadata_cf, "sequence"),
                None => db.get_cf(&metadata_cf, "sequence"),
            };

            match seq_data {
                Ok(Some(v)) => {
                    let seq_bytes: [u8; 8] = match v.try_into() {
                        Ok(v) => v,
                        Err(_) => return Err(IndexError::CorruptedData),
                    };
                    let sequence: u64 = u64::from_be_bytes(seq_bytes);

                    let source_change_id_data = match txn_guard.as_ref() {
                        Some(txn) => txn.get_cf(&metadata_cf, "source_change_id"),
                        None => db.get_cf(&metadata_cf, "source_change_id"),
                    };

                    let source_change_id: Arc<str> = match source_change_id_data {
                        Ok(Some(v)) => match String::from_utf8(v) {
                            Ok(v) => Arc::from(v),
                            Err(e) => return Err(IndexError::other(e)),
                        },
                        Ok(None) => Arc::from(""),
                        Err(e) => return Err(IndexError::other(e)),
                    };
                    Ok(ResultSequence {
                        sequence,
                        source_change_id,
                    })
                }
                Ok(None) => Ok(ResultSequence::default()),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }
}

impl ResultIndex for RocksDbResultIndex {}

pub(crate) fn get_lss_cf_options() -> Options {
    let mut lss_opts = Options::default();
    lss_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(8));
    lss_opts.set_merge_operator_associative("increment", increment_merge);
    lss_opts.set_compaction_filter("remove0", compact);
    lss_opts
}

pub(crate) fn get_value_cf_options() -> Options {
    let mut values_opts = Options::default();
    values_opts.optimize_for_point_lookup(VALUES_BLOCK_CACHE_SIZE);
    values_opts
}

pub(crate) fn get_metadata_cf_options() -> Options {
    let mut values_opts = Options::default();
    values_opts.optimize_for_point_lookup(1);
    values_opts
}

/// Collect all column family descriptors needed by the result index.
pub(crate) fn result_cf_descriptors() -> Vec<rocksdb::ColumnFamilyDescriptor> {
    vec![
        rocksdb::ColumnFamilyDescriptor::new(VALUES_CF, get_value_cf_options()),
        rocksdb::ColumnFamilyDescriptor::new(SETS_CF, get_lss_cf_options()),
        rocksdb::ColumnFamilyDescriptor::new(METADATA_CF, get_metadata_cf_options()),
    ]
}

fn encode_set_value_key(set_id: u64, value: OrderedFloat<f64>) -> [u8; 20] {
    let mut key = [0; 20];
    let set_id_bytes = set_id.to_be_bytes();
    let value_bytes = encode_sortable_f64(value.into_inner());
    key[0..8].clone_from_slice(&set_id_bytes);
    key[8..20].clone_from_slice(&value_bytes);
    key
}

fn encode_next_set_value_key(set_id: u64, value: OrderedFloat<f64>) -> [u8; 20] {
    let mut key = [0; 20];
    let set_id_bytes = set_id.to_be_bytes();
    let value_bytes = increment_sortable_f64(value.0);
    key[0..8].clone_from_slice(&set_id_bytes);
    key[8..20].clone_from_slice(&value_bytes);
    key
}

fn decode_set_value_key(key: &[u8]) -> Result<OrderedFloat<f64>, IndexError> {
    if key.len() != 20 {
        return Err(IndexError::CorruptedData);
    }
    let value_bytes: [u8; 12] = match key[8..20].try_into() {
        Ok(v) => v,
        Err(_) => return Err(IndexError::CorruptedData),
    };
    let val = decode_sortable_f64(&value_bytes);

    Ok(OrderedFloat(val))
}

fn encode_count(count: isize) -> Bytes {
    let mut buf = BytesMut::with_capacity(8);
    match (count as i64).encode(&mut buf) {
        Ok(_) => (),
        Err(_) => return Bytes::new(),
    }
    buf.freeze()
}

fn decode_count(count: &[u8]) -> Result<isize, IndexError> {
    let val = match <i64 as Message>::decode(count) {
        Ok(v) => v,
        Err(_) => return Err(IndexError::CorruptedData),
    };
    Ok(val as isize)
}

fn get_hash_key(owner: &ResultOwner, key: &ResultKey) -> u64 {
    let mut hasher = SpookyHasher::default();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

fn increment_merge(
    _new_key: &[u8],
    existing_val: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut sum = match existing_val {
        Some(v) => match decode_count(v) {
            Ok(v) => v,
            Err(_) => return None,
        },
        None => 0,
    };

    for op in operands {
        sum += match decode_count(op) {
            Ok(v) => v,
            Err(_) => return None,
        };
    }

    Some(encode_count(sum).to_vec())
}

/// remove zero value keys from the sorted set column family
fn compact(_level: u32, key: &[u8], value: &[u8]) -> rocksdb::compaction_filter::Decision {
    let val = match decode_count(value) {
        Ok(v) => v,
        Err(_) => return rocksdb::compaction_filter::Decision::Keep,
    };

    if val == 0 {
        log::debug!("Compacting zero value key: {:?}", &key);
        return rocksdb::compaction_filter::Decision::Remove;
    }

    rocksdb::compaction_filter::Decision::Keep
}

fn encode_sortable_f64(f: f64) -> [u8; 12] {
    let mut key = [0; 12];

    let int = {
        let i = f.trunc() as i128;
        (i + i64::MAX as i128) as u64
    };

    let frac = (f.abs().fract() * 1000000000.0).trunc() as u32;
    key[0..8].copy_from_slice(&int.to_be_bytes());
    key[8..12].copy_from_slice(&frac.to_be_bytes());
    key
}

fn decode_sortable_f64(key: &[u8; 12]) -> f64 {
    let int = {
        let mut buf = [0; 8];
        buf.copy_from_slice(&key[0..8]);
        let i = u64::from_be_bytes(buf);
        (i as i128 - i64::MAX as i128) as f64
    };

    let frac = {
        let mut buf = [0; 4];
        buf.copy_from_slice(&key[8..12]);
        let i = u32::from_be_bytes(buf);
        i as f64 / 1000000000.0
    };

    if int < 0.0 {
        int - frac
    } else {
        int + frac
    }
}

fn increment_sortable_f64(value: f64) -> [u8; 12] {
    let mut buf = [0; 16];
    let val_bytes = encode_sortable_f64(value);
    buf[4..16].copy_from_slice(&val_bytes);
    let mut i = u128::from_be_bytes(buf);
    i += 1;
    let mut result = [0; 12];
    result.copy_from_slice(&i.to_be_bytes()[4..16]);
    result
}
