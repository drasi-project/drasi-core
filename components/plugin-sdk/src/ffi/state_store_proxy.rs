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
        self.delete_many(store_id, &[key])
            .await
            .map(|count| count != 0)
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        // The legacy boolean slot conflates absence and errors. The get slot
        // already has distinct found/error fields without changing the ABI.
        self.get(store_id, key).await.map(|value| value.is_some())
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
        let keys: Vec<_> = keys.iter().map(|key| FfiStr::from_str(key)).collect();
        let count = unsafe {
            let vtable = &*self.vtable;
            (vtable.delete_many_fn)(
                vtable.state,
                FfiStr::from_str(store_id),
                keys.as_ptr(),
                keys.len(),
            )
        };
        usize::try_from(count)
            .ok()
            .filter(|count| *count <= keys.len())
            .ok_or_else(|| {
                drasi_lib::StateStoreError::Other(
                    "delete_many failed or returned an invalid count".into(),
                )
            })
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
        self.key_count(store_id).await.map(|count| count != 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::{FfiGetResult, FfiResult, FfiStringArray};
    use std::ffi::c_void;

    extern "C" fn get(_: *mut c_void, _: FfiStr, key: FfiStr) -> FfiGetResult {
        match unsafe { key.to_string() }.as_str() {
            "missing" => FfiGetResult::not_found(),
            "error" => FfiGetResult::err("storage unavailable".into()),
            _ => FfiGetResult::found(vec![1]),
        }
    }
    extern "C" fn set(_: *mut c_void, _: FfiStr, _: FfiStr, _: *const u8, _: usize) -> FfiResult {
        FfiResult::err("unused".into())
    }
    extern "C" fn boolean(_: *mut c_void, _: FfiStr, _: FfiStr) -> FfiResult {
        FfiResult::err("ambiguous legacy slot".into())
    }
    extern "C" fn get_many(
        _: *mut c_void,
        _: FfiStr,
        _: *const FfiStr,
        _: usize,
        _: *mut FfiGetResult,
    ) -> FfiResult {
        FfiResult::err("unused".into())
    }
    extern "C" fn set_many(
        _: *mut c_void,
        _: FfiStr,
        _: *const FfiStr,
        _: *const *const u8,
        _: *const usize,
        _: usize,
    ) -> FfiResult {
        FfiResult::err("unused".into())
    }
    extern "C" fn delete_many(_: *mut c_void, _: FfiStr, keys: *const FfiStr, count: usize) -> i64 {
        let mut deleted = 0;
        for key in unsafe { std::slice::from_raw_parts(keys, count) } {
            match unsafe { key.to_string() }.as_str() {
                "error" => return -1,
                "invalid" => return count as i64 + 1,
                "missing" => {}
                _ => deleted += 1,
            }
        }
        deleted
    }
    extern "C" fn count(_: *mut c_void, store: FfiStr) -> i64 {
        match unsafe { store.to_string() }.as_str() {
            "populated" => 2,
            "empty" => 0,
            _ => -1,
        }
    }
    extern "C" fn list(_: *mut c_void, _: FfiStr) -> FfiStringArray {
        FfiStringArray::from_vec(vec![])
    }
    extern "C" fn exists(_: *mut c_void, _: FfiStr) -> FfiResult {
        FfiResult::err("ambiguous legacy slot".into())
    }
    extern "C" fn sync(_: *mut c_void) -> FfiResult {
        FfiResult::ok()
    }
    extern "C" fn release(_: *mut c_void) {}

    #[tokio::test]
    async fn existence_and_deletion_preserve_absence_errors_and_exact_counts() {
        let table = StateStoreVtable {
            state: std::ptr::null_mut(),
            get_fn: get,
            set_fn: set,
            delete_fn: boolean,
            contains_key_fn: boolean,
            get_many_fn: get_many,
            set_many_fn: set_many,
            delete_many_fn: delete_many,
            clear_store_fn: count,
            list_keys_fn: list,
            store_exists_fn: exists,
            key_count_fn: count,
            sync_fn: sync,
            drop_fn: release,
        };
        let proxy = FfiStateStoreProxy { vtable: &table };
        assert!(proxy.contains_key("store", "present").await.unwrap());
        assert!(!proxy.contains_key("store", "missing").await.unwrap());
        assert!(proxy.contains_key("store", "error").await.is_err());
        assert!(proxy.store_exists("populated").await.unwrap());
        assert!(!proxy.store_exists("empty").await.unwrap());
        assert!(proxy.store_exists("error").await.is_err());
        assert!(proxy.delete("store", "present").await.unwrap());
        assert!(!proxy.delete("store", "missing").await.unwrap());
        assert_eq!(
            proxy
                .delete_many("store", &["one", "missing", "two"])
                .await
                .unwrap(),
            2
        );
        assert!(proxy.delete("store", "error").await.is_err());
        assert!(proxy.delete("store", "invalid").await.is_err());
    }
}
