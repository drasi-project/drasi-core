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

use crate::interface::SessionError;

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use caches::lru::CacheError;
use hashers::builtin::DefaultHasher;
use ordered_float::OrderedFloat;

use super::generation_cache::GenerationCache;
use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, IndexError, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter, SessionControl,
    },
};

pub struct CachedResultIndex {
    inner: Arc<dyn ResultIndex>,
    value_cache: GenerationCache<u64, ValueAccumulator>,
    set_count_cache: GenerationCache<(u64, OrderedFloat<f64>), isize>,
}

impl CachedResultIndex {
    pub fn new(inner: Arc<dyn ResultIndex>, cache_size: usize) -> Result<Self, CacheError> {
        Self::with_control(inner, cache_size, None)
    }

    pub fn new_with_session(
        inner: Arc<dyn ResultIndex>,
        cache_size: usize,
        session_control: Arc<dyn SessionControl>,
    ) -> Result<Self, CacheError> {
        Self::with_control(inner, cache_size, Some(session_control))
    }

    fn with_control(
        inner: Arc<dyn ResultIndex>,
        cache_size: usize,
        session_control: Option<Arc<dyn SessionControl>>,
    ) -> Result<Self, CacheError> {
        Ok(Self {
            inner,
            value_cache: GenerationCache::new(cache_size, session_control.clone())?,
            set_count_cache: GenerationCache::new(cache_size, session_control)?,
        })
    }
}

#[async_trait]
impl AccumulatorIndex for CachedResultIndex {
    async fn clear(&self) -> Result<(), IndexError> {
        let generation = self.value_cache.generation()?;
        self.inner.clear().await?;
        self.value_cache.clear(generation).await?;
        self.set_count_cache.clear(generation).await
    }

    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let cache_key = get_hash_key(owner, key);
        let (generation, cached) = self.value_cache.get(&cache_key).await?;
        if cached.is_some() {
            return Ok(cached);
        }
        let value = self.inner.get(key, owner).await?;
        self.value_cache.check(generation)?;
        if let Some(value) = &value {
            self.value_cache
                .put(generation, cache_key, value.clone())
                .await?;
        }
        Ok(value)
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let generation = self.value_cache.generation()?;
        let cache_key = get_hash_key(&owner, &key);
        self.inner.set(key, owner, value.clone()).await?;
        match value {
            Some(value) => self.value_cache.put(generation, cache_key, value).await,
            None => self.value_cache.remove(generation, &cache_key).await,
        }
    }
}

#[async_trait]
impl LazySortedSetStore for CachedResultIndex {
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let generation = self.value_cache.generation()?;
        let result = self.inner.get_next(set_id, value).await?;
        self.value_cache.check(generation)?;
        Ok(result)
    }

    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let key = (set_id, value);
        let (generation, cached) = self.set_count_cache.get(&key).await?;
        if let Some(value) = cached {
            return Ok(value);
        }
        let count = self.inner.get_value_count(set_id, value).await?;
        self.set_count_cache.put(generation, key, count).await?;
        Ok(count)
    }

    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        let generation = self.set_count_cache.generation()?;
        self.inner
            .increment_value_count(set_id, value, delta)
            .await?;
        // An uncached count is not zero. Reload the absolute backend value.
        self.set_count_cache
            .remove(generation, &(set_id, value))
            .await
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

fn get_hash_key(owner: &ResultOwner, key: &ResultKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        in_memory_index::in_memory_result_index::InMemoryResultIndex,
        interface::{NoOpSessionControl, SessionGuard},
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct DelayedIndex {
        inner: InMemoryResultIndex,
        reads: AtomicUsize,
        pause: AtomicBool,
        entered: Notify,
        release: Notify,
    }

    #[async_trait]
    impl AccumulatorIndex for DelayedIndex {
        async fn clear(&self) -> Result<(), IndexError> {
            self.inner.clear().await
        }

        async fn get(
            &self,
            key: &ResultKey,
            owner: &ResultOwner,
        ) -> Result<Option<ValueAccumulator>, IndexError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let value = self.inner.get(key, owner).await?;
            if self.pause.load(Ordering::Relaxed) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(value)
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
    impl LazySortedSetStore for DelayedIndex {
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

    #[async_trait]
    impl ResultSequenceCounter for DelayedIndex {
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

    impl ResultIndex for DelayedIndex {}

    #[tokio::test]
    async fn delayed_fill_cannot_cross_roots_and_active_root_reuses_reads() {
        let control = Arc::new(NoOpSessionControl);
        let inner = Arc::new(DelayedIndex {
            inner: InMemoryResultIndex::new(),
            reads: AtomicUsize::new(0),
            pause: AtomicBool::new(true),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let cache = Arc::new(
            CachedResultIndex::new_with_session(inner.clone(), 8, control.clone()).unwrap(),
        );
        let key = ResultKey::InputHash(1);
        let owner = ResultOwner::Function(1);
        inner
            .set(
                key.clone(),
                owner.clone(),
                Some(ValueAccumulator::Count { value: 1 }),
            )
            .await
            .unwrap();
        let old = SessionGuard::begin(control.clone()).await.unwrap();
        let task_cache = cache.clone();
        let delayed = tokio::spawn(async move {
            task_cache
                .get(&ResultKey::InputHash(1), &ResultOwner::Function(1))
                .await
        });
        inner.entered.notified().await;
        drop(old);
        let newer = SessionGuard::begin(control).await.unwrap();
        inner
            .set(
                key.clone(),
                owner.clone(),
                Some(ValueAccumulator::Count { value: 2 }),
            )
            .await
            .unwrap();
        inner.pause.store(false, Ordering::Relaxed);
        inner.release.notify_one();
        assert_eq!(
            delayed.await.unwrap().unwrap_err(),
            IndexError::other(SessionError::StaleCacheGeneration)
        );
        for _ in 0..3 {
            assert!(matches!(
                cache.get(&key, &owner).await.unwrap(),
                Some(ValueAccumulator::Count { value: 2 })
            ));
        }
        assert_eq!(inner.reads.load(Ordering::Relaxed), 2);
        newer.commit().await.unwrap();
        assert!(matches!(
            cache.get(&key, &owner).await.unwrap(),
            Some(ValueAccumulator::Count { value: 2 })
        ));
        assert_eq!(inner.reads.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn uncached_sorted_set_increment_preserves_existing_count() {
        let control = Arc::new(NoOpSessionControl);
        let inner = Arc::new(InMemoryResultIndex::new());
        let value = OrderedFloat(7.0);
        inner.increment_value_count(1, value, 10).await.unwrap();
        let cache = CachedResultIndex::new_with_session(inner, 8, control.clone()).unwrap();
        let guard = SessionGuard::begin(control).await.unwrap();
        cache.increment_value_count(1, value, 5).await.unwrap();
        assert_eq!(cache.get_value_count(1, value).await.unwrap(), 15);
        guard.commit().await.unwrap();
    }
}
