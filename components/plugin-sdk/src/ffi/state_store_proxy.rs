// Copyright 2025 The Drasi Authors.
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

//! Plugin-side state store proxy that wraps a `StateStoreVtable` into
//! a `StateStoreProvider` trait implementation.
//!
//! The host provides a `StateStoreVtable` (function pointers backed by its own
//! `Arc<dyn StateStoreProvider>`). The plugin wraps it in `FfiStateStoreProxy`
//! and uses it as a normal `StateStoreProvider`.

use std::collections::HashMap;

use super::types::FfiStr;
use super::vtables::StateStoreVtable;
use drasi_lib::{StateStoreProvider, StateStoreResult};

/// Plugin-side proxy: wraps a host-provided `StateStoreVtable` into a local
/// `StateStoreProvider` implementation.
pub struct FfiStateStoreProxy {
    pub(crate) vtable: *const StateStoreVtable,
}

unsafe impl Send for FfiStateStoreProxy {}
unsafe impl Sync for FfiStateStoreProxy {}

#[async_trait::async_trait]
impl StateStoreProvider for FfiStateStoreProxy {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        unsafe {
            let vtable = &*self.vtable;
            (vtable.get_fn)(
                vtable.state,
                FfiStr::from_str(store_id),
                FfiStr::from_str(key),
            )
            .into_result()
            .map_err(drasi_lib::StateStoreError::Other)
        }
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        unsafe {
            let vtable = &*self.vtable;
            (vtable.set_fn)(
                vtable.state,
                FfiStr::from_str(store_id),
                FfiStr::from_str(key),
                value.as_ptr(),
                value.len(),
            )
            .into_result()
            .map_err(drasi_lib::StateStoreError::Other)
        }
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        unsafe {
            let vtable = &*self.vtable;
            (vtable.delete_fn)(
                vtable.state,
                FfiStr::from_str(store_id),
                FfiStr::from_str(key),
            )
            .into_result()
            .map(|_| true)
            .map_err(drasi_lib::StateStoreError::Other)
        }
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.contains_key_fn)(
                vtable.state,
                FfiStr::from_str(store_id),
                FfiStr::from_str(key),
            );
            result.into_result().map(|_| true).or(Ok(false))
        }
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        // Fall back to individual gets for simplicity
        let mut result = HashMap::new();
        for key in keys {
            if let Some(val) = self.get(store_id, key).await? {
                result.insert(key.to_string(), val);
            }
        }
        Ok(result)
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        // Fall back to individual sets for simplicity
        for (key, value) in entries {
            self.set(store_id, key, value.to_vec()).await?;
        }
        Ok(())
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        let mut count = 0;
        for key in keys {
            if self.delete(store_id, key).await? {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.clear_store_fn)(vtable.state, FfiStr::from_str(store_id));
            if result < 0 {
                Err(drasi_lib::StateStoreError::Other(
                    "clear_store failed".into(),
                ))
            } else {
                Ok(result as usize)
            }
        }
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        unsafe {
            let vtable = &*self.vtable;
            let array = (vtable.list_keys_fn)(vtable.state, FfiStr::from_str(store_id));
            Ok(array.into_vec())
        }
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.store_exists_fn)(vtable.state, FfiStr::from_str(store_id));
            result.into_result().map(|_| true).or(Ok(false))
        }
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        unsafe {
            let vtable = &*self.vtable;
            let result = (vtable.key_count_fn)(vtable.state, FfiStr::from_str(store_id));
            if result < 0 {
                Err(drasi_lib::StateStoreError::Other("key_count failed".into()))
            } else {
                Ok(result as usize)
            }
        }
    }

    async fn sync(&self) -> StateStoreResult<()> {
        unsafe {
            let vtable = &*self.vtable;
            (vtable.sync_fn)(vtable.state)
                .into_result()
                .map_err(drasi_lib::StateStoreError::Other)
        }
    }
}
