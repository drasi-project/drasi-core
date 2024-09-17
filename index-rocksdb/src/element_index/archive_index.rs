use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use drasi_core::{
    interface::{ElementArchiveIndex, ElementStream, IndexError},
    models::{Element, ElementReference, ElementTimestamp, TimestampBound, TimestampRange},
};
use prost::{bytes::Bytes, Message};
use rocksdb::{OptimisticTransactionDB, Options, SliceTransform, Transaction};
use tokio::task;

use crate::{
    element_index::RocksDbElementIndex,
    storage_models::{StoredElement, StoredElementContainer},
};

use super::{Context, ReferenceHash};

pub const ARCHIVE_CF: &str = "archive";

#[async_trait]
impl ElementArchiveIndex for RocksDbElementIndex {
    #[allow(clippy::unwrap_used)]
    async fn get_element_as_at(
        &self,
        element_ref: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let context = self.context.clone();
        let element_key = super::hash_element_ref(element_ref);
        let mut key = element_key.to_vec();
        key.extend_from_slice(&time.to_be_bytes());

        let task = task::spawn_blocking(move || {
            let archive_cf = context.db.cf_handle(ARCHIVE_CF).expect("Archive CF not found");
            let mut iter = context
                .db
                .iterator_cf(
                    &archive_cf,
                    rocksdb::IteratorMode::From(&key, rocksdb::Direction::Reverse),
                )
                .take(1);
            if let Some(item) = iter.next() {
                match item {
                    Ok((_, value)) => {
                        return Ok(Some(value));
                    }
                    Err(err) => {
                        return Err(IndexError::other(err));
                    }
                }
            }
            Ok(None)
        });

        match task.await {
            Ok(Ok(Some(data))) => {
                let stored_element: StoredElement =
                    match StoredElementContainer::decode(data.as_ref()) {
                        Ok(container) => match container.element {
                            Some(element) => element,
                            None => return Err(IndexError::CorruptedData),
                        },
                        Err(e) => return Err(IndexError::other(e)),
                    };
                Ok(Some(Arc::new(stored_element.into())))
            }
            Ok(Ok(None)) => Ok(None),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[allow(clippy::unwrap_used)]
    async fn get_element_versions(
        &self,
        element_ref: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let context = self.context.clone();
        let element_key = super::hash_element_ref(element_ref);

        let from = range.from;
        let from_timestamp = match from {
            TimestampBound::Included(from) => from,
            TimestampBound::StartFromPrevious(from) => {
                let context = context.clone();
                let task = task::spawn_blocking(move || {
                    let archive_cf = context.db.cf_handle(ARCHIVE_CF).unwrap();
                    let mut start_key = element_key.clone().to_vec();
                    start_key.extend_from_slice(&from.to_be_bytes());
                    let mut iter = context
                        .db
                        .iterator_cf(
                            &archive_cf,
                            rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Reverse),
                        )
                        .take(1);
                    if let Some(item) = iter.next() {
                        match item {
                            Ok((key, _)) => {
                                let ts = match key.get(16..24) {
                                    // last 8 bytesis the timestamp
                                    Some(v) => u64::from_be_bytes(v.try_into().unwrap()),
                                    None => 0,
                                };
                                return Ok(ts);
                            }
                            Err(err) => {
                                return Err(IndexError::other(err));
                            }
                        }
                    }
                    Ok(0)
                });

                match task.await {
                    Ok(Ok(data)) => data,
                    Ok(Err(e)) => return Err(e),
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        };
        let to = range.to;
        let mut start_key = element_key.to_vec();

        task::spawn_blocking(move || {
            start_key.extend_from_slice(&from_timestamp.to_be_bytes());
            let archive_cf = context.db.cf_handle(ARCHIVE_CF).unwrap();

            for item in context.db.iterator_cf(
                &archive_cf,
                rocksdb::IteratorMode::From(&start_key, rocksdb::Direction::Forward),
            ) {
                match item {
                    Ok((key, data)) => {
                        if !key.starts_with(&element_key) {
                            break;
                        }
                        let ts = match key.get(16..24) {
                            Some(v) => u64::from_be_bytes(v.try_into().unwrap()),
                            None => {
                                tx.blocking_send(Err(IndexError::CorruptedData)).unwrap();
                                break;
                            }
                        };

                        if ts > to {
                            break;
                        }

                        let stored_element: StoredElement =
                            match StoredElementContainer::decode(data.as_ref()) {
                                Ok(container) => match container.element {
                                    Some(element) => element,
                                    None => continue,
                                },
                                Err(e) => {
                                    tx.blocking_send(Err(IndexError::other(e))).unwrap();
                                    break;
                                }
                            };
                        let element: Element = stored_element.into();
                        tx.blocking_send(Ok(Arc::new(element))).unwrap()
                    }
                    Err(e) => tx.blocking_send(Err(IndexError::other(e))).unwrap(),
                }
            }
        });

        Ok(Box::pin(stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        }))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let context = self.context.clone();

        let task = task::spawn_blocking(move || {
            if let Err(err) = context.db.drop_cf(ARCHIVE_CF) {
                return Err(IndexError::other(err));
            }
            if let Err(err) = context.db.create_cf(ARCHIVE_CF, &get_archive_cf_options()) {
                return Err(IndexError::other(err));
            }
            Ok(())
        });

        task.await.expect("Failed to clear archive")
    }
}

pub fn insert_archive(
    context: Arc<Context>,
    txn: &Transaction<OptimisticTransactionDB>,
    element_key: ReferenceHash,
    element: &Bytes,
    effective_from: u64,
) -> Result<(), IndexError> {
    let archive_cf = context.db.cf_handle(ARCHIVE_CF).expect("Archive CF not found");
    let mut key = element_key.to_vec();
    key.extend_from_slice(&effective_from.to_be_bytes());
    match txn.put_cf(&archive_cf, key, element) {
        Ok(_) => Ok(()),
        Err(err) => Err(IndexError::other(err)),
    }
}

pub fn get_archive_cf_options() -> Options {
    let mut opts = Options::default();
    opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(16));
    opts
}
