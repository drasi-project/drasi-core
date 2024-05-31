use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use caches::{lru::CacheError, Cache, DefaultHashBuilder, LRUCache};
use hashers::builtin::DefaultHasher;
use ordered_float::OrderedFloat;
use tokio::sync::RwLock;

use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, IndexError, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter,
    },
};

pub struct CachedResultIndex {
    inner: Arc<dyn ResultIndex>,

    value_cache: Arc<RwLock<LRUCache<u64, ValueAccumulator, DefaultHashBuilder>>>,
    set_count_cache: Arc<RwLock<LRUCache<(u64, OrderedFloat<f64>), isize, DefaultHashBuilder>>>,
}

impl CachedResultIndex {
    pub fn new(inner: Arc<dyn ResultIndex>, cache_size: usize) -> Result<Self, CacheError> {
        log::info!("using cached result index with cache size {}", cache_size);

        let value_cache = LRUCache::new(cache_size)?;
        let set_count_cache = LRUCache::new(cache_size)?;

        Ok(CachedResultIndex {
            inner,
            value_cache: Arc::new(RwLock::new(value_cache)),
            set_count_cache: Arc::new(RwLock::new(set_count_cache)),
        })
    }
}

#[async_trait]
impl AccumulatorIndex for CachedResultIndex {
    async fn clear(&self) -> Result<(), IndexError> {
        self.inner.clear().await?;

        let mut value_cache = self.value_cache.write().await;
        value_cache.purge();

        let mut set_count_cache = self.set_count_cache.write().await;
        set_count_cache.purge();

        Ok(())
    }

    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let cache_key = get_hash_key(owner, key);

        let mut cache = self.value_cache.write().await;
        match cache.get(&cache_key) {
            None => {
                let value = self.inner.get(key, owner).await?;
                match value {
                    None => Ok(None),
                    Some(v) => {
                        _ = cache.put(cache_key, v.clone());
                        Ok(Some(v))
                    }
                }
            }
            Some(v) => Ok(Some(v.clone())),
        }
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let cache_key = get_hash_key(&owner, &key);

        self.inner.set(key, owner, value.clone()).await?;

        let mut cache = self.value_cache.write().await;
        match value {
            None => _ = cache.remove(&cache_key),
            Some(v) => _ = cache.put(cache_key, v),
        };

        Ok(())
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
        let cache_key = (set_id, value);

        let mut cache = self.set_count_cache.write().await;
        match cache.get(&cache_key) {
            None => {
                let value = self.inner.get_value_count(set_id, value).await?;
                _ = cache.put(cache_key, value);
                Ok(value)
            }
            Some(v) => Ok(*v),
        }
    }

    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        self.inner
            .increment_value_count(set_id, value, delta)
            .await?;

        let cache_key = (set_id, value);
        let mut cache = self.set_count_cache.write().await;

        match cache.get_mut(&cache_key) {
            None => _ = cache.put(cache_key, delta),
            Some(v) => *v += delta,
        }

        Ok(())
    }
}

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

impl ResultIndex for CachedResultIndex {}

fn get_hash_key(owner: &ResultOwner, key: &ResultKey) -> u64 {
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}
