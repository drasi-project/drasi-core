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

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;

use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{IndexError, ResultIndex, ResultKey, ResultOwner, SessionControl, SessionGuard, TemporalIndex},
    models::{ElementReference, ElementValue},
};

use super::{codec, QueryEpoch, TemporalBatch, TemporalCatalog, TemporalKey, TemporalRecord};

// Retained state uses the existing result index transaction and value codec.
// Providers do not need another index field or a new persistence interface.
pub(crate) struct ResultTemporalIndex {
    index: Arc<dyn ResultIndex>,
}

const OWNER: ResultOwner = ResultOwner::Function(usize::MAX);
const DIRECTORY: &[u8] = b"drasi:temporal:directory";
const EMPTY_STORE: &[u8] = b"drasi:query-store:empty:v1";

/// Record a newly provisioned query store through the existing result-index protocol.
///
/// The provider must first verify that the entire query namespace is empty,
/// including graph, archive, timers, checkpoints, outbox and live results.
pub async fn initialize_empty_query_store(
    index: Arc<dyn ResultIndex>,
    control: Arc<dyn SessionControl>,
) -> Result<(), IndexError> {
    let root = SessionGuard::begin(control).await?;
    root.mark_dirty()?;
    ResultTemporalIndex::new(index).write(EMPTY_STORE, Some(vec![1])).await?;
    root.commit().await
}

pub(crate) async fn consume_empty_store_marker(
    index: Arc<dyn ResultIndex>,
    control: Arc<dyn SessionControl>,
) -> Result<bool, IndexError> {
    let root = SessionGuard::begin(control).await?;
    let index = ResultTemporalIndex::new(index);
    let empty = match index.read(EMPTY_STORE).await? {
        None => false,
        Some(value) if value == [1] => {
            root.mark_dirty()?;
            index.write(EMPTY_STORE, None).await?;
            true
        }
        Some(_) => return Err(IndexError::CorruptedData),
    };
    root.commit().await?;
    Ok(empty)
}

impl ResultTemporalIndex {
    pub(crate) fn new(index: Arc<dyn ResultIndex>) -> Self {
        Self { index }
    }

    fn key(bytes: &[u8]) -> ResultKey {
        ResultKey::Element(ElementReference::new(
            "\0drasi:temporal",
            &codec::key_field(bytes),
        ))
    }

    async fn read(&self, key: &[u8]) -> Result<Option<Vec<u8>>, IndexError> {
        match self.index.get(&Self::key(key), &OWNER).await? {
            None => Ok(None),
            Some(ValueAccumulator::Value(ElementValue::String(value))) => Ok(Some(
                codec::decode_field(&value).map_err(IndexError::other)?,
            )),
            Some(_) => Err(IndexError::CorruptedData),
        }
    }

    async fn write(&self, key: &[u8], value: Option<Vec<u8>>) -> Result<(), IndexError> {
        let value = value.map(|bytes| {
            ValueAccumulator::Value(ElementValue::String(Arc::from(codec::key_field(&bytes))))
        });
        self.index.set(Self::key(key), OWNER, value).await
    }

    async fn directory(&self) -> Result<BTreeSet<Vec<u8>>, IndexError> {
        match self.read(DIRECTORY).await? {
            Some(bytes) => rmp_serde::from_slice(&bytes).map_err(IndexError::other),
            None => Ok(BTreeSet::new()),
        }
    }

    async fn remove_keys(&self, epoch: Option<QueryEpoch>) -> Result<(), IndexError> {
        let mut keys = self.directory().await?;
        let prefix = epoch.map(codec::epoch_prefix);
        let removed: Vec<_> = keys
            .iter()
            .filter(|key| prefix.as_ref().is_none_or(|prefix| key.starts_with(prefix)))
            .cloned()
            .collect();
        for key in &removed {
            keys.remove(key);
        }
        let directory = rmp_serde::to_vec(&keys).map_err(IndexError::other)?;
        for key in removed {
            self.write(&key, None).await?;
        }
        self.write(
            DIRECTORY,
            if keys.is_empty() {
                None
            } else {
                Some(directory)
            },
        )
        .await?;
        if epoch.is_none() {
            self.write(codec::CATALOG_KEY, None).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl TemporalIndex for ResultTemporalIndex {
    async fn load_catalog(&self) -> Result<Option<TemporalCatalog>, IndexError> {
        self.read(codec::CATALOG_KEY)
            .await?
            .map(|bytes| codec::decode_catalog(&bytes).map_err(IndexError::other))
            .transpose()
    }

    async fn store_catalog(&self, catalog: TemporalCatalog) -> Result<(), IndexError> {
        let bytes = codec::encode_catalog(&catalog).map_err(IndexError::other)?;
        self.write(codec::CATALOG_KEY, Some(bytes)).await
    }

    async fn get(&self, key: &TemporalKey) -> Result<Option<TemporalRecord>, IndexError> {
        let encoded = codec::encode_key(key).map_err(IndexError::other)?;
        self.read(&encoded)
            .await?
            .map(|bytes| codec::decode_record(key, &bytes).map_err(IndexError::other))
            .transpose()
    }

    async fn apply(&self, batch: TemporalBatch) -> Result<(), IndexError> {
        let writes = codec::prepare_batch(&batch).map_err(IndexError::other)?;
        let mut keys = self.directory().await?;
        for (key, value) in &writes {
            if value.is_some() {
                keys.insert(key.clone());
            } else {
                keys.remove(key);
            }
        }
        let directory = rmp_serde::to_vec(&keys).map_err(IndexError::other)?;
        for (key, value) in writes {
            self.write(&key, value).await?;
        }
        self.write(DIRECTORY, Some(directory)).await
    }

    async fn clear_epoch(&self, epoch: QueryEpoch) -> Result<(), IndexError> {
        self.remove_keys(Some(epoch)).await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.remove_keys(None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        evaluation::temporal::{fixtures, EpochState},
        in_memory_index::in_memory_result_index::InMemoryResultIndex,
        interface::AccumulatorIndex,
    };

    #[tokio::test]
    async fn retained_state_reopens_through_the_original_result_index() {
        let store = Arc::new(InMemoryResultIndex::new());
        let index = ResultTemporalIndex::new(store.clone());
        let namespace = fixtures::namespace();
        let mut input = fixtures::input();
        let ticket = fixtures::ticket(&mut input);
        input.tickets.insert(ticket.id.clone(), ticket);
        let mut batch = TemporalBatch::new(namespace);
        batch
            .put(TemporalRecord::Epoch(EpochState::new(namespace)))
            .unwrap();
        batch.put(TemporalRecord::Input(input.clone())).unwrap();
        index.apply(batch).await.unwrap();
        let reopened = ResultTemporalIndex::new(store.clone());
        let restored = reopened
            .get(&TemporalKey::Input(input.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            codec::encode_record(&restored).unwrap(),
            codec::encode_record(&TemporalRecord::Input(input)).unwrap()
        );
        reopened.clear_epoch(namespace.epoch).await.unwrap();
        assert!(index
            .get(&TemporalKey::Epoch(namespace))
            .await
            .unwrap()
            .is_none());
        store
            .set(
                ResultKey::InputHash(1),
                ResultOwner::Function(0),
                Some(ValueAccumulator::Count { value: 7 }),
            )
            .await
            .unwrap();
        reopened.clear().await.unwrap();
        assert!(matches!(
            store
                .get(&ResultKey::InputHash(1), &ResultOwner::Function(0))
                .await
                .unwrap(),
            Some(ValueAccumulator::Count { value: 7 })
        ));
    }
}
