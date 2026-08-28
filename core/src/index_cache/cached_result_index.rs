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

use std::sync::Arc;

use async_trait::async_trait;
use caches::lru::CacheError;
use ordered_float::OrderedFloat;

use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, IndexError, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter,
    },
};

pub struct CachedResultIndex {
    inner: Arc<dyn ResultIndex>,
}

impl CachedResultIndex {
    pub fn new(inner: Arc<dyn ResultIndex>, _cache_size: usize) -> Result<Self, CacheError> {
        // Result-index writes are transactional, but this wrapper has no commit/rollback hook.
        // Keep it as a compatibility wrapper rather than serving rolled-back accumulator state.
        Ok(CachedResultIndex { inner })
    }
}

#[async_trait]
impl AccumulatorIndex for CachedResultIndex {
    async fn is_empty(&self) -> Result<bool, IndexError> {
        self.inner.is_empty().await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.inner.clear().await
    }

    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        self.inner.get(key, owner).await
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        self.inner.set(key, owner, value).await
    }
}

#[async_trait]
impl LazySortedSetStore for CachedResultIndex {
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        self.inner.get_next(set_id, value).await
    }

    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        self.inner.get_value_count(set_id, value).await
    }

    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        self.inner.increment_value_count(set_id, value, delta).await
    }
}

impl ResultIndex for CachedResultIndex {}

#[async_trait]
impl ResultSequenceCounter for CachedResultIndex {
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        self.inner.apply_sequence(sequence, source_change_id).await
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        self.inner.get_sequence().await
    }
}
