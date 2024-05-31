use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use drasi_core::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};
use hashers::jenkins::spooky_hash::SpookyHasher;
use prost::{bytes::Bytes, Message};
use rocksdb::{
    AsColumnFamilyRef, OptimisticTransactionDB, Options, ReadOptions, SliceTransform, Transaction,
};
use tokio::task;

use crate::storage_models::{StoredElementReference, StoredFutureElementRef};

/// RocksDB future queue
///
/// Column Families
///     - fqueue [due_time + hash(future_element_ref)] -> future_element_ref {8 byte prefix (due_time)}
///     - findex [(position_in_query + group_signature) + hash(future_element_ref)] -> due_time {12 byte prefix (position_in_query(4) + group_signature(8))}
pub struct RocksDbFutureQueue {
    db: Arc<OptimisticTransactionDB>,
}

const QUEUE_CF: &str = "fqueue";
const INDEX_CF: &str = "findex";

impl RocksDbFutureQueue {
    pub fn new(query_id: &str, path: &str) -> Result<Self, IndexError> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_db_write_buffer_size(1024 * 1024);

        let path = std::path::PathBuf::from(path).join(query_id).join("fqi");
        let path = match path.to_str() {
            Some(path) => path,
            None => return Err(IndexError::NotSupported),
        };

        let db = match OptimisticTransactionDB::open_cf_descriptors(
            &opts,
            path,
            vec![
                rocksdb::ColumnFamilyDescriptor::new(QUEUE_CF, get_fqueue_cf_options()),
                rocksdb::ColumnFamilyDescriptor::new(INDEX_CF, get_findex_cf_options()),
            ],
        ) {
            Ok(db) => db,
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(Self { db: Arc::new(db) })
    }
}

#[async_trait]
impl FutureQueue for RocksDbFutureQueue {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let db = self.db.clone();

        let stored_element_ref: StoredElementReference = element_ref.into();

        let task = task::spawn_blocking(move || {
            let index_cf = db.cf_handle(INDEX_CF).unwrap();
            let queue_cf = db.cf_handle(QUEUE_CF).unwrap();

            let future_ref = StoredFutureElementRef {
                element_ref: stored_element_ref,
                original_time,
                due_time,
                position_in_query: position_in_query as u32,
                group_signature,
            };

            let hash = {
                let mut h = SpookyHasher::default();
                let _ = &future_ref.hash(&mut h);
                h.finish().to_be_bytes()
            };

            let prefix = encode_index_prefix(position_in_query as u32, group_signature);

            let txn = db.transaction();

            let should_push = match push_type {
                PushType::Always => true,
                PushType::IfNotExists => {
                    let mut iter = db.prefix_iterator_cf(&index_cf, &prefix);
                    match iter.next() {
                        Some(item) => match item {
                            Ok((key, _)) => key[0..12] != prefix,
                            Err(e) => return Err(IndexError::other(e)),
                        },
                        None => true,
                    }
                }
                PushType::Overwrite => {
                    remove_internal(&txn, &index_cf, &queue_cf, prefix)?;
                    true
                }
            };

            if should_push {
                let index_key = {
                    let mut buf = [0u8; 20];
                    buf[0..12].copy_from_slice(&prefix);
                    buf[12..20].copy_from_slice(&hash);
                    buf
                };

                let queue_key = {
                    let mut buf = [0u8; 16];
                    buf[0..8].copy_from_slice(&due_time.to_be_bytes());
                    buf[8..16].copy_from_slice(&hash);
                    buf
                };

                if let Err(e) = txn.put_cf(&index_cf, &index_key, &due_time.to_be_bytes()) {
                    return Err(IndexError::other(e));
                };

                if let Err(e) = txn.put_cf(&queue_cf, &queue_key, &future_ref.encode_to_vec()) {
                    return Err(IndexError::other(e));
                };

                if let Err(e) = txn.commit() {
                    return Err(IndexError::other(e));
                }
            }

            Ok(should_push)
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let db = self.db.clone();

        let task = task::spawn_blocking(move || {
            let index_cf = db.cf_handle(INDEX_CF).unwrap();
            let queue_cf = db.cf_handle(QUEUE_CF).unwrap();

            let position_in_query = position_in_query as u32;

            let prefix = encode_index_prefix(position_in_query, group_signature);

            let txn = db.transaction();

            remove_internal(&txn, &index_cf, &queue_cf, prefix)?;

            match txn.commit() {
                Ok(_) => Ok(()),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let db = self.db.clone();

        let task = task::spawn_blocking(move || {
            let index_cf = db.cf_handle(INDEX_CF).unwrap();
            let queue_cf = db.cf_handle(QUEUE_CF).unwrap();

            let read_opts = ReadOptions::default();

            let mut iter = db.iterator_cf_opt(&queue_cf, read_opts, rocksdb::IteratorMode::Start);

            let txn = db.transaction();

            let result = {
                match iter.next() {
                    Some(head) => match head {
                        Ok((key, future_ref)) => {
                            let hash = &key[8..];

                            let stored_future_ref =
                                match StoredFutureElementRef::decode(Bytes::from(future_ref)) {
                                    Ok(v) => v,
                                    Err(_) => return Err(IndexError::CorruptedData),
                                };

                            if let Err(e) = txn.delete_cf(&queue_cf, &key) {
                                return Err(IndexError::other(e));
                            };

                            let prefix = encode_index_prefix(
                                stored_future_ref.position_in_query,
                                stored_future_ref.group_signature,
                            );
                            let index_key = {
                                let mut buf = [0u8; 20];
                                buf[0..12].copy_from_slice(&prefix);
                                buf[12..20].copy_from_slice(&hash);
                                buf
                            };

                            if let Err(e) = txn.delete_cf(&index_cf, &index_key) {
                                return Err(IndexError::other(e));
                            };

                            let future_ref: FutureElementRef = stored_future_ref.into();
                            Some(future_ref)
                        }
                        Err(e) => return Err(IndexError::other(e)),
                    },
                    _ => None,
                }
            };

            match txn.commit() {
                Ok(_) => Ok(result),
                Err(e) => Err(IndexError::other(e)),
            }
        });

        match task.await {
            Ok(v) => v,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let db = self.db.clone();

        let task = task::spawn_blocking(move || {
            let queue_cf = db.cf_handle(QUEUE_CF).unwrap();

            let read_opts = ReadOptions::default();

            let mut iter = db.iterator_cf_opt(&queue_cf, read_opts, rocksdb::IteratorMode::Start);

            if let Some(head) = iter.next() {
                match head {
                    Ok((key, _)) => {
                        let due_time = u64::from_be_bytes(match key[0..8].try_into() {
                            Ok(v) => v,
                            Err(_) => return Err(IndexError::CorruptedData),
                        });
                        return Ok(Some(due_time));
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            } else {
                return Ok(None);
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
            if let Err(err) = db.drop_cf(QUEUE_CF) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = db.create_cf(QUEUE_CF, &get_fqueue_cf_options()) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = db.drop_cf(INDEX_CF) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = db.create_cf(INDEX_CF, &get_findex_cf_options()) {
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

fn remove_internal(
    txn: &Transaction<OptimisticTransactionDB>,
    index_cf: &impl AsColumnFamilyRef,
    queue_cf: &impl AsColumnFamilyRef,
    index_prefix: [u8; 12],
) -> Result<(), IndexError> {
    let mut iter = txn.prefix_iterator_cf(index_cf, index_prefix);

    while let Some(item) = iter.next() {
        match item {
            Ok((key, due_time)) => {
                if key[0..12] != index_prefix {
                    break;
                }

                let hash = &key[12..];
                let queue_key = [&due_time, hash].concat();

                match txn.delete_cf(queue_cf, &queue_key) {
                    Ok(_) => {}
                    Err(e) => return Err(IndexError::other(e)),
                };

                match txn.delete_cf(index_cf, &key) {
                    Ok(_) => {}
                    Err(e) => return Err(IndexError::other(e)),
                };
            }
            Err(e) => return Err(IndexError::other(e)),
        }
    }
    Ok(())
}

fn encode_index_prefix(position_in_query: u32, group_signature: u64) -> [u8; 12] {
    let mut buf = [0u8; 12];
    buf[0..4].copy_from_slice(&position_in_query.to_be_bytes());
    buf[4..12].copy_from_slice(&group_signature.to_be_bytes());
    buf
}

fn get_fqueue_cf_options() -> Options {
    let mut opts = Options::default();
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(8));
    opts
}

fn get_findex_cf_options() -> Options {
    let mut opts = Options::default();
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(12));
    opts
}
