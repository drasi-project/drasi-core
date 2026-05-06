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
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    ops::Bound,
    sync::Arc,
};

use bytes::Bytes;

use crate::interface::{
    ResultCheckpoint, ResultIndex, ResultKey, ResultOwner, ResultSequence, ResultSequenceCounter,
};
use async_trait::async_trait;
use hashers::builtin::DefaultHasher;
use ordered_float::OrderedFloat;
use tokio::sync::RwLock;

use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{AccumulatorIndex, IndexError, LazySortedSetStore},
};

pub struct InMemoryResultIndex {
    values: Arc<RwLock<HashMap<u64, ValueAccumulator>>>,
    sorted_sets: Arc<RwLock<HashMap<u64, BTreeMap<OrderedFloat<f64>, isize>>>>,
    checkpoint: Arc<RwLock<ResultCheckpoint>>,
}

impl Default for InMemoryResultIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryResultIndex {
    pub fn new() -> Self {
        InMemoryResultIndex {
            values: Arc::new(RwLock::new(HashMap::new())),
            sorted_sets: Arc::new(RwLock::new(HashMap::new())),
            checkpoint: Arc::new(RwLock::new(ResultCheckpoint::default())),
        }
    }
}

#[async_trait]
impl AccumulatorIndex for InMemoryResultIndex {
    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let data = self.values.read().await;
        let hash_key = get_hash_key(owner, key);
        match data.get(&hash_key) {
            None => return Ok(None),
            Some(f) => return Ok(Some(f.clone())),
        }
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let hash_key = get_hash_key(&owner, &key);
        let mut data = self.values.write().await;
        match value {
            None => {
                data.remove(&hash_key);
                Ok(())
            }
            Some(v) => {
                data.insert(hash_key, v);
                Ok(())
            }
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut data = self.values.write().await;
        data.clear();

        let mut data = self.sorted_sets.write().await;
        data.clear();

        Ok(())
    }
}

fn get_hash_key(owner: &ResultOwner, key: &ResultKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

#[async_trait]
impl LazySortedSetStore for InMemoryResultIndex {
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<ordered_float::OrderedFloat<f64>>,
    ) -> Result<Option<(ordered_float::OrderedFloat<f64>, isize)>, crate::interface::IndexError>
    {
        let data = self.sorted_sets.read().await;
        match data.get(&set_id) {
            None => Ok(None),
            Some(set) => match value {
                None => match set.iter().next() {
                    None => Ok(None),
                    Some((k, v)) => Ok(Some((*k, *v))),
                },
                Some(v) => match set.range((Bound::Excluded(v), Bound::Unbounded)).next() {
                    None => Ok(None),
                    Some((k, v)) => Ok(Some((*k, *v))),
                },
            },
        }
    }

    async fn get_value_count(
        &self,
        set_id: u64,
        value: ordered_float::OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let data = self.sorted_sets.read().await;
        match data.get(&set_id) {
            None => Ok(0),
            Some(set) => match set.get(&value) {
                None => Ok(0),
                Some(count) => Ok(*count),
            },
        }
    }

    async fn increment_value_count(
        &self,
        set_id: u64,
        value: ordered_float::OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        if delta == 0 {
            return Ok(());
        }

        let mut data = self.sorted_sets.write().await;
        let set = data.entry(set_id).or_insert(BTreeMap::new());

        let entry = set
            .entry(value)
            .and_modify(|v| *v += delta)
            .or_insert(delta);

        if entry.is_negative() {
            return Err(IndexError::CorruptedData);
        }

        if *entry == 0 {
            set.remove(&value);
        }

        Ok(())
    }
}

#[async_trait]
impl ResultSequenceCounter for InMemoryResultIndex {
    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let data = self.checkpoint.read().await;
        Ok(ResultSequence {
            sequence: data.sequence,
            source_id: data.source_id.clone(),
        })
    }

    async fn apply_checkpoint(
        &self,
        sequence: u64,
        source_id: &str,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let mut data = self.checkpoint.write().await;
        data.sequence = sequence;
        data.source_id = Arc::from(source_id);
        // Upsert into per-source position map
        if let Some(pos) = source_position {
            data.source_positions
                .insert(Arc::from(source_id), pos.clone());
        } else {
            data.source_positions.remove(source_id);
        }
        Ok(())
    }

    async fn get_checkpoint(&self) -> Result<ResultCheckpoint, IndexError> {
        let data = self.checkpoint.read().await;
        Ok(data.clone())
    }

    async fn get_source_position(&self, source_id: &str) -> Result<Option<Bytes>, IndexError> {
        let data = self.checkpoint.read().await;
        Ok(data.source_positions.get(source_id).cloned())
    }
}

impl ResultIndex for InMemoryResultIndex {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_checkpoint_round_trip() {
        let index = InMemoryResultIndex::new();

        // Initially default
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 0);
        assert!(cp.source_positions.is_empty());

        // Apply with 8-byte position for source "src-1"
        let pos = Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8]);
        index
            .apply_checkpoint(5, "src-1", Some(&pos))
            .await
            .unwrap();

        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 5);
        assert_eq!(cp.source_id.as_ref(), "src-1");
        assert_eq!(cp.get_source_position("src-1"), Some(&pos));

        // Apply with position for a second source — first source position preserved
        let pos2 = Bytes::from_static(&[9, 10, 11, 12]);
        index
            .apply_checkpoint(6, "src-2", Some(&pos2))
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 6);
        assert_eq!(cp.get_source_position("src-1"), Some(&pos));
        assert_eq!(cp.get_source_position("src-2"), Some(&pos2));

        // Apply with None position removes that source's entry
        index.apply_checkpoint(10, "src-1", None).await.unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 10);
        assert_eq!(cp.get_source_position("src-1"), None);
        assert_eq!(cp.get_source_position("src-2"), Some(&pos2));

        // Apply with large position (100 bytes)
        let big_pos = Bytes::from(vec![0xAB; 100]);
        index
            .apply_checkpoint(20, "src-cosmos", Some(&big_pos))
            .await
            .unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 20);
        assert_eq!(cp.get_source_position("src-cosmos"), Some(&big_pos));
    }

    #[tokio::test]
    async fn test_sequence_and_checkpoint_consistency() {
        let index = InMemoryResultIndex::new();

        // apply_checkpoint(None) should be visible via get_checkpoint
        index.apply_checkpoint(7, "change-a", None).await.unwrap();
        let cp = index.get_checkpoint().await.unwrap();
        assert_eq!(cp.sequence, 7);
        assert_eq!(cp.source_id.as_ref(), "change-a");

        // apply_checkpoint should be visible via get_sequence
        let pos = Bytes::from_static(b"position-data");
        index
            .apply_checkpoint(15, "change-b", Some(&pos))
            .await
            .unwrap();
        let seq = index.get_sequence().await.unwrap();
        assert_eq!(seq.sequence, 15);
        assert_eq!(seq.source_id.as_ref(), "change-b");
    }
}
