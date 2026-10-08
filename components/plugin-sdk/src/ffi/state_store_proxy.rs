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

impl FfiStateStoreProxy {
    /// Take ownership of a host-provided ABI 0.17 state-store vtable.
    ///
    /// # Safety
    /// `vtable` must be a non-null, uniquely transferred `Box<StateStoreVtable>`
    /// with valid callbacks and state. It must not be a borrowed/ABI 0.16 table.
    pub unsafe fn new(vtable: *const StateStoreVtable) -> Self {
        assert!(!vtable.is_null(), "state-store vtable must not be null");
        Self { vtable }
    }

    /// Read the declaration without hiding an unsupported or failed description.
    pub fn try_durability(&self) -> StateStoreResult<drasi_core::interface::StorageDurability> {
        unsafe {
            let vtable = &*self.vtable;
            let mut out = std::mem::MaybeUninit::uninit();
            (vtable.durability_fn)(vtable.state, out.as_mut_ptr())
                .into_result()
                .map_err(drasi_lib::StateStoreError::Other)?;
            out.assume_init()
                .try_into()
                .map_err(drasi_lib::StateStoreError::Other)
        }
    }
}

impl Drop for FfiStateStoreProxy {
    fn drop(&mut self) {
        unsafe {
            let vtable = Box::from_raw(self.vtable as *mut StateStoreVtable);
            (vtable.drop_fn)(vtable.state);
        }
    }
}

unsafe impl Send for FfiStateStoreProxy {}
unsafe impl Sync for FfiStateStoreProxy {}

#[async_trait::async_trait]
impl StateStoreProvider for FfiStateStoreProxy {
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.try_durability().unwrap_or_else(|error| {
            log::error!("State-store durability could not be established: {error}");
            drasi_core::interface::StorageDurability::UNKNOWN
        })
    }

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
            let mut array = std::mem::MaybeUninit::uninit();
            (vtable.list_keys_fn)(vtable.state, FfiStr::from_str(store_id), array.as_mut_ptr())
                .into_result()
                .map_err(drasi_lib::StateStoreError::Other)?;
            Ok(array.assume_init().into_vec())
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
    extern "C" fn list(_: *mut c_void, store: FfiStr, out_keys: *mut FfiStringArray) -> FfiResult {
        let keys = match unsafe { store.to_string() }.as_str() {
            "populated" => vec!["first".into(), "second".into()],
            "empty" => vec![],
            _ => return FfiResult::err("storage unavailable".into()),
        };
        unsafe { out_keys.write(FfiStringArray::from_vec(keys)) };
        FfiResult::ok()
    }
    extern "C" fn exists(_: *mut c_void, _: FfiStr) -> FfiResult {
        FfiResult::err("ambiguous legacy slot".into())
    }
    extern "C" fn sync(_: *mut c_void) -> FfiResult {
        FfiResult::ok()
    }
    extern "C" fn release(_: *mut c_void) {}
    extern "C" fn durability(
        _: *mut c_void,
        out: *mut super::super::durability::FfiStorageDurability,
    ) -> FfiResult {
        unsafe { out.write(drasi_core::interface::StorageDurability::VOLATILE.into()) };
        FfiResult::ok()
    }

    fn test_table() -> StateStoreVtable {
        StateStoreVtable {
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
            durability_fn: durability,
        }
    }

    #[test]
    fn abi_016_prefix_offsets_are_preserved() {
        let table = test_table();
        let base = &table as *const StateStoreVtable as usize;
        let width = std::mem::size_of::<*mut c_void>();
        for (index, address) in [
            std::ptr::addr_of!(table.state) as usize,
            std::ptr::addr_of!(table.get_fn) as usize,
            std::ptr::addr_of!(table.set_fn) as usize,
            std::ptr::addr_of!(table.delete_fn) as usize,
            std::ptr::addr_of!(table.contains_key_fn) as usize,
            std::ptr::addr_of!(table.get_many_fn) as usize,
            std::ptr::addr_of!(table.set_many_fn) as usize,
            std::ptr::addr_of!(table.delete_many_fn) as usize,
            std::ptr::addr_of!(table.clear_store_fn) as usize,
            std::ptr::addr_of!(table.list_keys_fn) as usize,
            std::ptr::addr_of!(table.store_exists_fn) as usize,
            std::ptr::addr_of!(table.key_count_fn) as usize,
            std::ptr::addr_of!(table.sync_fn) as usize,
            std::ptr::addr_of!(table.drop_fn) as usize,
            std::ptr::addr_of!(table.durability_fn) as usize,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(address - base, index * width);
        }
        assert_eq!(std::mem::size_of::<StateStoreVtable>(), 15 * width);
    }

    #[test]
    fn failed_or_invalid_durability_descriptions_are_not_evidence() {
        use super::super::durability::FfiStorageDurability;
        use drasi_core::interface::StorageDurability;

        extern "C" fn failed(_: *mut c_void, _: *mut FfiStorageDurability) -> FfiResult {
            FfiResult::err("description unavailable".into())
        }
        extern "C" fn invalid(_: *mut c_void, out: *mut FfiStorageDurability) -> FfiResult {
            unsafe {
                out.write(FfiStorageDurability {
                    version: 2,
                    process_restart: 2,
                    power_loss: 2,
                    storage_loss: 2,
                })
            };
            FfiResult::ok()
        }
        for callback in [failed as extern "C" fn(_, _) -> _, invalid] {
            let mut table = test_table();
            table.durability_fn = callback;
            let proxy = unsafe { FfiStateStoreProxy::new(Box::into_raw(Box::new(table))) };
            assert!(proxy.try_durability().is_err());
            assert_eq!(proxy.durability(), StorageDurability::UNKNOWN);
        }
    }

    #[tokio::test]
    async fn existence_and_deletion_preserve_absence_errors_and_exact_counts() {
        let table = test_table();
        let proxy = unsafe { FfiStateStoreProxy::new(Box::into_raw(Box::new(table))) };
        assert_eq!(
            proxy.durability(),
            drasi_core::interface::StorageDurability::VOLATILE
        );
        assert!(proxy.contains_key("store", "present").await.unwrap());
        assert!(!proxy.contains_key("store", "missing").await.unwrap());
        assert!(proxy.contains_key("store", "error").await.is_err());
        assert!(proxy.store_exists("populated").await.unwrap());
        assert!(!proxy.store_exists("empty").await.unwrap());
        assert!(proxy.store_exists("error").await.is_err());
        assert_eq!(
            proxy.list_keys("populated").await.unwrap(),
            vec!["first", "second"]
        );
        assert!(proxy.list_keys("empty").await.unwrap().is_empty());
        assert!(proxy
            .list_keys("error")
            .await
            .unwrap_err()
            .to_string()
            .contains("storage unavailable"));
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
