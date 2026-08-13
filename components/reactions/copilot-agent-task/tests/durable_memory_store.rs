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

//! A `MemoryStateStoreProvider` wrapper that reports itself as durable.
//!
//! The Copilot Agent Task reaction requires (`is_durable() == true`) a state
//! store the host considers durable, since reservation/execution records
//! must genuinely survive restarts for idempotency to hold. Exercising that
//! *logic* in tests does not require real disk persistence (unlike, say,
//! testing an actual crash/restart of the OS process) — the tests in this
//! directory simulate "restart" by reusing the same `Arc<dyn
//! StateStoreProvider>` across a fresh reaction/pipeline build, which an
//! in-memory store models perfectly as long as the host's durability gate
//! is satisfied. Hence this thin delegating wrapper, test-only.

use async_trait::async_trait;
use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreProvider, StateStoreResult};
use std::collections::HashMap;

pub struct DurableMemoryStateStoreProvider {
    inner: MemoryStateStoreProvider,
}

impl DurableMemoryStateStoreProvider {
    pub fn new() -> Self {
        Self {
            inner: MemoryStateStoreProvider::new(),
        }
    }
}

impl Default for DurableMemoryStateStoreProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StateStoreProvider for DurableMemoryStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store_id, key).await
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        self.inner.set(store_id, key, value).await
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        self.inner.set_many(store_id, entries).await
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.key_count(store_id).await
    }

    fn is_durable(&self) -> bool {
        true
    }
}
