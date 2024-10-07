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

use crate::interface::{
    ResultIndex, ResultKey, ResultOwner, ResultSequence, ResultSequenceCounter,
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
    sequence: Arc<RwLock<ResultSequence>>,
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
            sequence: Arc::new(RwLock::new(ResultSequence::default())),
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
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        let mut data = self.sequence.write().await;
        data.sequence = sequence;
        data.source_change_id = Arc::from(source_change_id);
        Ok(())
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let data = self.sequence.read().await;
        Ok(data.clone())
    }
}

impl ResultIndex for InMemoryResultIndex {}
