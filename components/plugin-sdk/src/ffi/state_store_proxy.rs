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

impl Drop for FfiStateStoreProxy {
    fn drop(&mut self) {
        if self.vtable.is_null() {
            return;
        }
        unsafe {
            let vtable = &*self.vtable;
            // Free the inner state (Box<Arc<dyn StateStoreProvider>>)
            (vtable.drop_fn)(vtable.state);
            // Free the vtable struct itself (allocated via Box::into_raw by the
            // host when it built the FfiRuntimeContext)
            let _ = Box::from_raw(self.vtable as *mut StateStoreVtable);
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::{FfiGetResult, FfiResult, FfiStringArray};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    static DROP_CALLS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn stub_get(_state: *mut c_void, _store_id: FfiStr, _key: FfiStr) -> FfiGetResult {
        FfiGetResult::not_found()
    }

    extern "C" fn stub_set(
        _state: *mut c_void,
        _store_id: FfiStr,
        _key: FfiStr,
        _value: *const u8,
        _value_len: usize,
    ) -> FfiResult {
        FfiResult::ok()
    }

    extern "C" fn stub_delete(_state: *mut c_void, _store_id: FfiStr, _key: FfiStr) -> FfiResult {
        FfiResult::ok()
    }

    extern "C" fn stub_contains_key(
        _state: *mut c_void,
        _store_id: FfiStr,
        _key: FfiStr,
    ) -> FfiResult {
        FfiResult::ok()
    }

    extern "C" fn stub_get_many(
        _state: *mut c_void,
        _store_id: FfiStr,
        _keys: *const FfiStr,
        _keys_count: usize,
        _out_values: *mut FfiGetResult,
    ) -> FfiResult {
        FfiResult::ok()
    }

    extern "C" fn stub_set_many(
        _state: *mut c_void,
        _store_id: FfiStr,
        _keys: *const FfiStr,
        _values: *const *const u8,
        _value_lens: *const usize,
        _count: usize,
    ) -> FfiResult {
        FfiResult::ok()
    }

    extern "C" fn stub_delete_many(
        _state: *mut c_void,
        _store_id: FfiStr,
        _keys: *const FfiStr,
        _keys_count: usize,
    ) -> i64 {
        0
    }

    extern "C" fn stub_clear_store(_state: *mut c_void, _store_id: FfiStr) -> i64 {
        0
    }

    extern "C" fn stub_list_keys(_state: *mut c_void, _store_id: FfiStr) -> FfiStringArray {
        FfiStringArray::from_vec(Vec::new())
    }

    extern "C" fn stub_store_exists(_state: *mut c_void, _store_id: FfiStr) -> FfiResult {
        FfiResult::ok()
    }

    extern "C" fn stub_key_count(_state: *mut c_void, _store_id: FfiStr) -> i64 {
        0
    }

    extern "C" fn stub_sync(_state: *mut c_void) -> FfiResult {
        FfiResult::ok()
    }

    /// Mirrors the host's `ss_drop`: reclaims the boxed `Arc` behind `state`.
    extern "C" fn counting_drop(state: *mut c_void) {
        DROP_CALLS.fetch_add(1, Ordering::SeqCst);
        unsafe { drop(Box::from_raw(state as *mut Arc<()>)) };
    }

    fn test_vtable(state: *mut c_void) -> StateStoreVtable {
        StateStoreVtable {
            state,
            get_fn: stub_get,
            set_fn: stub_set,
            delete_fn: stub_delete,
            contains_key_fn: stub_contains_key,
            get_many_fn: stub_get_many,
            set_many_fn: stub_set_many,
            delete_many_fn: stub_delete_many,
            clear_store_fn: stub_clear_store,
            list_keys_fn: stub_list_keys,
            store_exists_fn: stub_store_exists,
            key_count_fn: stub_key_count,
            sync_fn: stub_sync,
            drop_fn: counting_drop,
        }
    }

    #[test]
    fn drop_invokes_vtable_drop_fn_and_releases_state() {
        DROP_CALLS.store(0, Ordering::SeqCst);

        // Stands in for the host's `Box<Arc<dyn StateStoreProvider>>`; the strong
        // count shows whether the provider reference is actually released.
        let provider = Arc::new(());
        let state = Box::into_raw(Box::new(provider.clone())) as *mut c_void;
        assert_eq!(Arc::strong_count(&provider), 2);

        let vtable = Box::into_raw(Box::new(test_vtable(state)));
        let proxy = FfiStateStoreProxy {
            vtable: vtable as *const _,
        };

        drop(proxy);

        assert_eq!(DROP_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(
            Arc::strong_count(&provider),
            1,
            "provider reference must be released when the proxy is dropped"
        );
    }

    #[test]
    fn drop_is_a_noop_for_a_null_vtable() {
        let proxy = FfiStateStoreProxy {
            vtable: std::ptr::null(),
        };

        drop(proxy);
    }
}
