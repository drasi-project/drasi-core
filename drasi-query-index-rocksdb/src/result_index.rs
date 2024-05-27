use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use drasi_query_core::{
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

/// RocksDB accumulator index store
///
/// Column Families
///     - values [set_id] -> ValueAccumulator
///     - sorted-sets [set_id + value] -> count {8 byte prefix (set_id)}
pub struct RocksDbResultIndex {
    db: Arc<OptimisticTransactionDB>,
}

const VALUES_CF: &str = "values";
const SETS_CF: &str = "sorted-sets";
const METADATA_CF: &str = "metadata";
const VALUES_BLOCK_CACHE_SIZE: u64 = 32;

impl RocksDbResultIndex {
    pub fn new(query_id: &str, path: &str) -> Result<Self, IndexError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_db_write_buffer_size(32 * 1024 * 1024);

        let path = std::path::PathBuf::from(path)
            .join(query_id)
            .join("aggregations");
        let path = match path.to_str() {
            Some(path) => path,
            None => return Err(IndexError::NotSupported),
        };

        let db = match OptimisticTransactionDB::open_cf_descriptors(
            &opts,
            path,
            vec![
                rocksdb::ColumnFamilyDescriptor::new(VALUES_CF, get_value_cf_options()),
                rocksdb::ColumnFamilyDescriptor::new(SETS_CF, get_lss_cf_options()),
                rocksdb::ColumnFamilyDescriptor::new(METADATA_CF, get_metadata_cf_options()),
            ],
        ) {
            Ok(db) => db,
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(RocksDbResultIndex { db: Arc::new(db) })
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
        let set_id = get_hash_key(owner, key);

        let task = task::spawn_blocking(move || {
            let values_cf = db.cf_handle(VALUES_CF).unwrap();
            match db.get_cf(&values_cf, set_id.to_be_bytes()) {
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
        let set_id = get_hash_key(&owner, &key);

        let task = task::spawn_blocking(move || {
            let values_cf = db.cf_handle(VALUES_CF).unwrap();
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

        let task = task::spawn_blocking(move || {
            let set_cf = db.cf_handle(SETS_CF).unwrap();
            let prefix = set_id.to_be_bytes();

            match value {
                Some(value) => {
                    let mut iter = db.iterator_cf(
                        &set_cf,
                        IteratorMode::From(
                            &encode_next_set_value_key(set_id, value),
                            Direction::Forward,
                        ),
                    );
                    while let Some(item) = iter.next() {
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
                    let mut iter = db.prefix_iterator_cf(&set_cf, &prefix);
                    while let Some(item) = iter.next() {
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
        let task = task::spawn_blocking(move || {
            let set_cf = db.cf_handle(SETS_CF).unwrap();
            let key = encode_set_value_key(set_id, value);
            match db.get_cf(&set_cf, &key) {
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

        let task = task::spawn_blocking(move || {
            let set_cf = db.cf_handle(SETS_CF).unwrap();
            let key = encode_set_value_key(set_id, value);
            match db.merge_cf(&set_cf, &key, encode_count(delta)) {
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
        let source_change_id = source_change_id.to_string();
        let task = task::spawn_blocking(move || {
            let metadata_cf = db.cf_handle(METADATA_CF).unwrap();
            let mut batch = rocksdb::WriteBatchWithTransaction::default();
            batch.put_cf(&metadata_cf, "sequence", &sequence.to_be_bytes());
            batch.put_cf(
                &metadata_cf,
                "source_change_id",
                source_change_id.as_bytes(),
            );
            match db.write(batch) {
                Ok(_) => Ok(()),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let db = self.db.clone();
        let task = task::spawn_blocking(move || {
            let metadata_cf = db.cf_handle(METADATA_CF).unwrap();
            match db.get_cf(&metadata_cf, "sequence") {
                Ok(Some(v)) => {
                    let seq_bytes: [u8; 8] = match v.try_into() {
                        Ok(v) => v,
                        Err(_) => return Err(IndexError::CorruptedData),
                    };
                    let sequence: u64 = u64::from_be_bytes(seq_bytes);

                    let source_change_id: Arc<str> =
                        match db.get_cf(&metadata_cf, "source_change_id") {
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

fn get_lss_cf_options() -> Options {
    let mut lss_opts = Options::default();
    lss_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(8));
    lss_opts.set_merge_operator_associative("increment", increment_merge);
    lss_opts.set_compaction_filter("remove0", compact);
    lss_opts
}

fn get_value_cf_options() -> Options {
    let mut values_opts = Options::default();
    values_opts.optimize_for_point_lookup(VALUES_BLOCK_CACHE_SIZE);
    values_opts
}

fn get_metadata_cf_options() -> Options {
    let mut values_opts = Options::default();
    values_opts.optimize_for_point_lookup(1);
    values_opts
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
    let value_bytes: [u8; 12] = key[8..20].try_into().unwrap();
    let val = decode_sortable_f64(&value_bytes);

    Ok(OrderedFloat(val))
}

fn encode_count(count: isize) -> Bytes {
    let mut buf = BytesMut::with_capacity(8);
    (count as i64).encode(&mut buf).unwrap();
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
