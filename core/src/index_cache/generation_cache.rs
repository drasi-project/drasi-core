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

use crate::interface::SessionError;

use std::{hash::Hash, sync::Arc};

use caches::{lru::CacheError, Cache, DefaultHashBuilder, LRUCache};
use tokio::sync::{Mutex, MutexGuard};

use crate::interface::{CacheGeneration, IndexError, SessionControl};

type CacheState<K, V> = (Option<CacheGeneration>, LRUCache<K, V, DefaultHashBuilder>);

pub(super) struct GenerationCache<K: Hash + Eq, V> {
    control: Option<Arc<dyn SessionControl>>,
    state: Mutex<CacheState<K, V>>,
}

impl<K: Hash + Eq, V: Clone> GenerationCache<K, V> {
    pub fn new(size: usize, control: Option<Arc<dyn SessionControl>>) -> Result<Self, CacheError> {
        Ok(Self {
            control,
            state: Mutex::new((None, LRUCache::new(size)?)),
        })
    }

    pub fn generation(&self) -> Result<CacheGeneration, IndexError> {
        match &self.control {
            Some(control) => crate::interface::session_tracker(control)?.cache_generation(),
            None => Ok(CacheGeneration::untracked()),
        }
    }

    pub fn check(&self, generation: CacheGeneration) -> Result<(), IndexError> {
        if self.generation()? != generation {
            return Err(IndexError::other(SessionError::StaleCacheGeneration));
        }
        Ok(())
    }

    async fn lock(
        &self,
        generation: CacheGeneration,
    ) -> Result<MutexGuard<'_, CacheState<K, V>>, IndexError> {
        let mut state = self.state.lock().await;
        self.check(generation)?;
        if state.0 != Some(generation) {
            state.1.purge();
            state.0 = Some(generation);
        }
        Ok(state)
    }

    pub async fn get(&self, key: &K) -> Result<(CacheGeneration, Option<V>), IndexError> {
        let generation = self.generation()?;
        if self.control.is_none() {
            return Ok((generation, None));
        }
        let value = self.lock(generation).await?.1.get(key).cloned();
        Ok((generation, value))
    }

    pub async fn put(
        &self,
        generation: CacheGeneration,
        key: K,
        value: V,
    ) -> Result<(), IndexError> {
        if self.control.is_none() {
            return Ok(());
        }
        self.lock(generation).await?.1.put(key, value);
        Ok(())
    }

    pub async fn remove(&self, generation: CacheGeneration, key: &K) -> Result<(), IndexError> {
        self.lock(generation).await?.1.remove(key);
        Ok(())
    }

    pub async fn clear(&self, generation: CacheGeneration) -> Result<(), IndexError> {
        self.lock(generation).await?.1.purge();
        Ok(())
    }
}
