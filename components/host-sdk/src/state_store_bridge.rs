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

//! Bridge from host-side `StateStoreProvider` to FFI `StateStoreVtable`.
//!
//! The host creates a `StateStoreVtable` wrapping its real `StateStoreProvider`
//! and passes it to plugins via `FfiRuntimeContext`. Plugins use the vtable
//! through `FfiStateStoreProxy` (in the plugin SDK) to access persistent state.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_lib::StateStoreProvider;
use drasi_plugin_sdk::ffi::{
    FfiGetResult, FfiResult, FfiStorageDurability, FfiStr, FfiStringArray, StateStoreVtable,
};

/// Wraps an FFI body in `catch_unwind` and returns `default` on panic.
///
/// Panics unwinding across an `extern "C"` boundary are undefined behavior
/// (and on most modern toolchains immediately abort the process). All
/// extern "C" entry points exposed by this bridge MUST funnel through this
/// helper. The default value is what the host returns to the plugin on panic.
fn ffi_guard<T, F: FnOnce() -> T>(default: impl FnOnce() -> T, f: F) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => default(),
    }
}

/// Builds a `StateStoreVtable` from a host-side `Arc<dyn StateStoreProvider>`.
pub struct StateStoreVtableBuilder;

impl StateStoreVtableBuilder {
    /// Build a `StateStoreVtable` that dispatches to the given `StateStoreProvider`.
    ///
    /// The returned vtable holds an `Arc` clone — the provider stays alive as long
    /// as the vtable (or any plugin holding it) is alive.
    pub fn build(provider: Arc<dyn StateStoreProvider>) -> StateStoreVtable {
        // Store as Box<Arc<dyn StateStoreProvider>> to preserve the fat pointer
        let boxed = Box::new(provider);
        let state = Box::into_raw(boxed) as *mut c_void;
        StateStoreVtable {
            state,
            get_fn: ss_get,
            set_fn: ss_set,
            delete_fn: ss_delete,
            contains_key_fn: ss_contains_key,
            get_many_fn: ss_get_many,
            set_many_fn: ss_set_many,
            delete_many_fn: ss_delete_many,
            clear_store_fn: ss_clear_store,
            list_keys_fn: ss_list_keys,
            store_exists_fn: ss_store_exists,
            key_count_fn: ss_key_count,
            sync_fn: ss_sync,
            drop_fn: ss_drop,
            durability_fn: ss_durability,
        }
    }
}

fn provider_ref(state: *mut c_void) -> &'static dyn StateStoreProvider {
    let arc = unsafe { &*(state as *const Arc<dyn StateStoreProvider>) };
    arc.as_ref()
}

extern "C" fn ss_durability(state: *mut c_void, out: *mut FfiStorageDurability) -> FfiResult {
    if state.is_null() || out.is_null() {
        return FfiResult::err("ss_durability: null state or output".into());
    }
    ffi_guard(
        || FfiResult::err("ss_durability: panic".into()),
        || {
            let declaration = FfiStorageDurability::from(provider_ref(state).durability());
            unsafe { out.write(declaration) };
            FfiResult::ok()
        },
    )
}

fn block_on<F: std::future::Future>(f: F) -> Option<F::Output> {
    // Use a current-thread runtime to avoid nesting issues with the host's runtime.
    // Returns None if the runtime cannot be built — callers convert to an FFI error
    // rather than panicking across the boundary.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    Some(rt.block_on(f))
}

extern "C" fn ss_get(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiGetResult {
    ffi_guard(
        || FfiGetResult::err("ss_get: panic".into()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let key = unsafe { key.to_string() };
            match block_on(provider.get(&store_id, &key)) {
                Some(Ok(Some(value))) => FfiGetResult::found(value),
                Some(Ok(None)) => FfiGetResult::not_found(),
                Some(Err(error)) => FfiGetResult::err(error.to_string()),
                None => FfiGetResult::err("failed to build runtime".into()),
            }
        },
    )
}

extern "C" fn ss_set(
    state: *mut c_void,
    store_id: FfiStr,
    key: FfiStr,
    value: *const u8,
    value_len: usize,
) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_set: panic".to_string()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let key = unsafe { key.to_string() };
            let value = unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec();
            match block_on(provider.set(&store_id, &key, value)) {
                Some(Ok(())) => FfiResult::ok(),
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_delete(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_delete: panic".to_string()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let key = unsafe { key.to_string() };
            match block_on(provider.delete(&store_id, &key)) {
                Some(Ok(_)) => FfiResult::ok(),
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_contains_key(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_contains_key: panic".to_string()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let key = unsafe { key.to_string() };
            match block_on(provider.contains_key(&store_id, &key)) {
                Some(Ok(true)) => FfiResult::ok(),
                Some(Ok(false)) => FfiResult::err("not_found".to_string()),
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_get_many(
    state: *mut c_void,
    store_id: FfiStr,
    keys: *const FfiStr,
    keys_count: usize,
    out_values: *mut FfiGetResult,
) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_get_many: panic".to_string()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let key_strs: Vec<String> = (0..keys_count)
                .map(|i| unsafe { (*keys.add(i)).to_string() })
                .collect();
            let key_refs: Vec<&str> = key_strs.iter().map(|s| s.as_str()).collect();
            match block_on(provider.get_many(&store_id, &key_refs)) {
                Some(Ok(results)) => {
                    for (i, key) in key_strs.iter().enumerate() {
                        let ffi_result = match results.get(key) {
                            Some(value) => FfiGetResult::found(value.clone()),
                            None => FfiGetResult::not_found(),
                        };
                        unsafe { *out_values.add(i) = ffi_result };
                    }
                    FfiResult::ok()
                }
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_set_many(
    state: *mut c_void,
    store_id: FfiStr,
    keys: *const FfiStr,
    values: *const *const u8,
    value_lens: *const usize,
    count: usize,
) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_set_many: panic".to_string()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let entries: Vec<(String, Vec<u8>)> = (0..count)
                .map(|i| unsafe {
                    let key = (*keys.add(i)).to_string();
                    let len = *value_lens.add(i);
                    let val = std::slice::from_raw_parts(*values.add(i), len).to_vec();
                    (key, val)
                })
                .collect();
            let refs: Vec<(&str, &[u8])> = entries
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            match block_on(provider.set_many(&store_id, &refs)) {
                Some(Ok(())) => FfiResult::ok(),
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_delete_many(
    state: *mut c_void,
    store_id: FfiStr,
    keys: *const FfiStr,
    keys_count: usize,
) -> i64 {
    ffi_guard(
        || -1,
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            let key_strs: Vec<String> = (0..keys_count)
                .map(|i| unsafe { (*keys.add(i)).to_string() })
                .collect();
            let key_refs: Vec<&str> = key_strs.iter().map(|s| s.as_str()).collect();
            match block_on(provider.delete_many(&store_id, &key_refs)) {
                Some(Ok(count)) => count as i64,
                _ => -1,
            }
        },
    )
}

extern "C" fn ss_clear_store(state: *mut c_void, store_id: FfiStr) -> i64 {
    ffi_guard(
        || -1,
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            match block_on(provider.clear_store(&store_id)) {
                Some(Ok(count)) => count as i64,
                _ => -1,
            }
        },
    )
}

extern "C" fn ss_list_keys(
    state: *mut c_void,
    store_id: FfiStr,
    out_keys: *mut FfiStringArray,
) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_list_keys: panic".into()),
        || {
            if out_keys.is_null() {
                return FfiResult::err("ss_list_keys: null output".into());
            }
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            match block_on(provider.list_keys(&store_id)) {
                Some(Ok(keys)) => {
                    unsafe { out_keys.write(FfiStringArray::from_vec(keys)) };
                    FfiResult::ok()
                }
                Some(Err(error)) => FfiResult::err(error.to_string()),
                None => FfiResult::err("failed to build runtime".into()),
            }
        },
    )
}

extern "C" fn ss_store_exists(state: *mut c_void, store_id: FfiStr) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_store_exists: panic".to_string()),
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            match block_on(provider.store_exists(&store_id)) {
                Some(Ok(true)) => FfiResult::ok(),
                Some(Ok(false)) => FfiResult::err("not_found".to_string()),
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_key_count(state: *mut c_void, store_id: FfiStr) -> i64 {
    ffi_guard(
        || -1,
        || {
            let provider = provider_ref(state);
            let store_id = unsafe { store_id.to_string() };
            match block_on(provider.key_count(&store_id)) {
                Some(Ok(count)) => count as i64,
                _ => -1,
            }
        },
    )
}

extern "C" fn ss_sync(state: *mut c_void) -> FfiResult {
    ffi_guard(
        || FfiResult::err("ss_sync: panic".to_string()),
        || {
            let provider = provider_ref(state);
            match block_on(provider.sync()) {
                Some(Ok(())) => FfiResult::ok(),
                Some(Err(e)) => FfiResult::err(e.to_string()),
                None => FfiResult::err("failed to build runtime".to_string()),
            }
        },
    )
}

extern "C" fn ss_drop(state: *mut c_void) {
    ffi_guard(
        || log::error!("State-store provider panicked during release"),
        || {
            // Reconstruct the Box<Arc<...>> and drop it
            unsafe { drop(Box::from_raw(state as *mut Arc<dyn StateStoreProvider>)) };
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::{MemoryStateStoreProvider, StateStoreError, StateStoreResult};
    use std::collections::HashMap;

    struct FailedRead(bool);

    #[async_trait::async_trait]
    impl StateStoreProvider for FailedRead {
        fn durability(&self) -> drasi_core::interface::StorageDurability {
            assert!(!self.0, "injected durability panic");
            drasi_core::interface::StorageDurability::UNKNOWN
        }

        async fn get(&self, _: &str, _: &str) -> StateStoreResult<Option<Vec<u8>>> {
            assert!(!self.0, "injected provider panic");
            Err(StateStoreError::Other("injected storage outage".into()))
        }
        async fn set(&self, _: &str, _: &str, _: Vec<u8>) -> StateStoreResult<()> {
            unreachable!()
        }
        async fn delete(&self, _: &str, _: &str) -> StateStoreResult<bool> {
            unreachable!()
        }
        async fn contains_key(&self, _: &str, _: &str) -> StateStoreResult<bool> {
            unreachable!()
        }
        async fn get_many(
            &self,
            _: &str,
            _: &[&str],
        ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
            unreachable!()
        }
        async fn set_many(&self, _: &str, _: &[(&str, &[u8])]) -> StateStoreResult<()> {
            unreachable!()
        }
        async fn delete_many(&self, _: &str, _: &[&str]) -> StateStoreResult<usize> {
            unreachable!()
        }
        async fn clear_store(&self, _: &str) -> StateStoreResult<usize> {
            unreachable!()
        }
        async fn list_keys(&self, _: &str) -> StateStoreResult<Vec<String>> {
            assert!(!self.0, "injected provider panic");
            Err(StateStoreError::Other("injected storage outage".into()))
        }
        async fn store_exists(&self, _: &str) -> StateStoreResult<bool> {
            unreachable!()
        }
        async fn key_count(&self, _: &str) -> StateStoreResult<usize> {
            unreachable!()
        }
    }

    #[test]
    fn read_errors_and_panics_are_not_missing_checkpoints() {
        for panic in [false, true] {
            let table = StateStoreVtableBuilder::build(Arc::new(FailedRead(panic)));
            let result = unsafe {
                (table.get_fn)(
                    table.state,
                    FfiStr::from_str("query"),
                    FfiStr::from_str("checkpoint"),
                )
                .into_result()
            };
            let error = result.expect_err("a failed checkpoint read must not look absent");
            assert!(
                error.contains(if panic { "panic" } else { "storage outage" }),
                "{error}"
            );
            (table.drop_fn)(table.state);
        }
    }

    #[test]
    fn values_absence_and_bridge_provider_release_remain_distinct() {
        let provider = Arc::new(MemoryStateStoreProvider::new());
        let weak = Arc::downgrade(&provider);
        let table = StateStoreVtableBuilder::build(provider.clone());
        drop(provider);
        let store = || FfiStr::from_str("store");
        let key = || FfiStr::from_str("present");
        assert_eq!(
            unsafe { (table.get_fn)(table.state, store(), key()).into_result() }.unwrap(),
            None
        );
        let mut keys = std::mem::MaybeUninit::uninit();
        unsafe { (table.list_keys_fn)(table.state, store(), keys.as_mut_ptr()).into_result() }
            .unwrap();
        assert!(unsafe { keys.assume_init().into_vec() }.is_empty());
        unsafe { (table.set_fn)(table.state, store(), key(), b"saved".as_ptr(), 5).into_result() }
            .unwrap();
        assert_eq!(
            unsafe { (table.get_fn)(table.state, store(), key()).into_result() }.unwrap(),
            Some(b"saved".to_vec())
        );
        let mut keys = std::mem::MaybeUninit::uninit();
        unsafe { (table.list_keys_fn)(table.state, store(), keys.as_mut_ptr()).into_result() }
            .unwrap();
        assert_eq!(unsafe { keys.assume_init().into_vec() }, vec!["present"]);
        assert!(weak.upgrade().is_some());
        (table.drop_fn)(table.state);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn list_errors_and_panics_are_not_empty_history() {
        for panic in [false, true] {
            let table = StateStoreVtableBuilder::build(Arc::new(FailedRead(panic)));
            let mut keys = FfiStringArray::from_vec(vec!["untouched".into()]);
            let error = unsafe {
                (table.list_keys_fn)(table.state, FfiStr::from_str("store"), &mut keys)
                    .into_result()
            }
            .expect_err("failed history lookup");
            assert!(
                error.contains(if panic { "panic" } else { "storage outage" }),
                "{error}"
            );
            assert_eq!(unsafe { keys.into_vec() }, vec!["untouched"]);
            (table.drop_fn)(table.state);
        }
    }

    #[test]
    fn durability_and_provider_ownership_survive_the_actual_proxy_boundary() {
        use drasi_core::interface::StorageDurability;
        use drasi_plugin_sdk::ffi::state_store_proxy::FfiStateStoreProxy;

        let provider = Arc::new(MemoryStateStoreProvider::new());
        let weak = Arc::downgrade(&provider);
        let table = StateStoreVtableBuilder::build(provider.clone());
        let proxy = Arc::new(unsafe { FfiStateStoreProxy::new(Box::into_raw(Box::new(table))) });
        drop(provider);
        assert_eq!(proxy.try_durability().unwrap(), StorageDurability::VOLATILE);
        let retained = proxy.clone();
        drop(proxy);
        assert!(weak.upgrade().is_some());
        drop(retained);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn actual_persistent_and_unknown_provider_declarations_cross_the_bridge() {
        use drasi_core::interface::StorageDurability;
        use drasi_plugin_sdk::ffi::state_store_proxy::FfiStateStoreProxy;

        let directory = tempfile::tempdir().unwrap();
        let persistent = drasi_state_store_redb::RedbStateStoreProvider::new(
            directory.path().join("state.redb"),
        )
        .unwrap();
        for (provider, expected) in [
            (
                Arc::new(persistent) as Arc<dyn StateStoreProvider>,
                StorageDurability::LOCAL_POWER_LOSS,
            ),
            (Arc::new(FailedRead(false)), StorageDurability::UNKNOWN),
        ] {
            let table = StateStoreVtableBuilder::build(provider);
            let proxy = unsafe { FfiStateStoreProxy::new(Box::into_raw(Box::new(table))) };
            assert_eq!(proxy.try_durability().unwrap(), expected);
        }
    }

    #[test]
    fn durability_failure_does_not_initialize_output_or_grant_a_guarantee() {
        use drasi_core::interface::StorageDurability;
        use drasi_plugin_sdk::ffi::state_store_proxy::FfiStateStoreProxy;

        let table = StateStoreVtableBuilder::build(Arc::new(FailedRead(true)));
        let mut output = FfiStorageDurability::from(StorageDurability::LOCAL_POWER_LOSS);
        assert!(unsafe { (table.durability_fn)(table.state, &mut output).into_result() }.is_err());
        assert_eq!(
            StorageDurability::try_from(output).unwrap(),
            StorageDurability::LOCAL_POWER_LOSS
        );
        assert!(
            unsafe { (table.durability_fn)(table.state, std::ptr::null_mut()).into_result() }
                .is_err()
        );
        let proxy = unsafe { FfiStateStoreProxy::new(Box::into_raw(Box::new(table))) };
        assert!(proxy.try_durability().is_err());
        assert_eq!(proxy.durability(), StorageDurability::UNKNOWN);
    }

    #[test]
    fn panic_defaults_allocate_only_on_failure() {
        let calls = std::cell::Cell::new(0);
        let fallback = || {
            calls.set(calls.get() + 1);
            2
        };
        assert_eq!(ffi_guard(fallback, || 1), 1);
        assert_eq!(calls.get(), 0);
        assert_eq!(ffi_guard(fallback, || panic!("injected")), 2);
        assert_eq!(calls.get(), 1);
    }
}
