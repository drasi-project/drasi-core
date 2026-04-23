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
use drasi_plugin_sdk::ffi::{FfiGetResult, FfiResult, FfiStr, FfiStringArray, StateStoreVtable};

/// Wraps an FFI body in `catch_unwind` and returns `default` on panic.
///
/// Panics unwinding across an `extern "C"` boundary are undefined behavior
/// (and on most modern toolchains immediately abort the process). All
/// extern "C" entry points exposed by this bridge MUST funnel through this
/// helper. The default value is what the host returns to the plugin on panic.
fn ffi_guard<T, F: FnOnce() -> T>(default: T, f: F) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => default,
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
        }
    }
}

fn provider_ref(state: *mut c_void) -> &'static dyn StateStoreProvider {
    let arc = unsafe { &*(state as *const Arc<dyn StateStoreProvider>) };
    arc.as_ref()
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
    ffi_guard(FfiGetResult::not_found(), || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        let key = unsafe { key.to_string() };
        match block_on(provider.get(&store_id, &key)) {
            Some(Ok(Some(value))) => FfiGetResult::found(value),
            _ => FfiGetResult::not_found(),
        }
    })
}

extern "C" fn ss_set(
    state: *mut c_void,
    store_id: FfiStr,
    key: FfiStr,
    value: *const u8,
    value_len: usize,
) -> FfiResult {
    ffi_guard(FfiResult::err("ss_set: panic".to_string()), || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        let key = unsafe { key.to_string() };
        let value = unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec();
        match block_on(provider.set(&store_id, &key, value)) {
            Some(Ok(())) => FfiResult::ok(),
            Some(Err(e)) => FfiResult::err(e.to_string()),
            None => FfiResult::err("failed to build runtime".to_string()),
        }
    })
}

extern "C" fn ss_delete(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult {
    ffi_guard(FfiResult::err("ss_delete: panic".to_string()), || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        let key = unsafe { key.to_string() };
        match block_on(provider.delete(&store_id, &key)) {
            Some(Ok(_)) => FfiResult::ok(),
            Some(Err(e)) => FfiResult::err(e.to_string()),
            None => FfiResult::err("failed to build runtime".to_string()),
        }
    })
}

extern "C" fn ss_contains_key(state: *mut c_void, store_id: FfiStr, key: FfiStr) -> FfiResult {
    ffi_guard(FfiResult::err("ss_contains_key: panic".to_string()), || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        let key = unsafe { key.to_string() };
        match block_on(provider.contains_key(&store_id, &key)) {
            Some(Ok(true)) => FfiResult::ok(),
            Some(Ok(false)) => FfiResult::err("not_found".to_string()),
            Some(Err(e)) => FfiResult::err(e.to_string()),
            None => FfiResult::err("failed to build runtime".to_string()),
        }
    })
}

extern "C" fn ss_get_many(
    state: *mut c_void,
    store_id: FfiStr,
    keys: *const FfiStr,
    keys_count: usize,
    out_values: *mut FfiGetResult,
) -> FfiResult {
    ffi_guard(FfiResult::err("ss_get_many: panic".to_string()), || {
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
    })
}

extern "C" fn ss_set_many(
    state: *mut c_void,
    store_id: FfiStr,
    keys: *const FfiStr,
    values: *const *const u8,
    value_lens: *const usize,
    count: usize,
) -> FfiResult {
    ffi_guard(FfiResult::err("ss_set_many: panic".to_string()), || {
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
    })
}

extern "C" fn ss_delete_many(
    state: *mut c_void,
    store_id: FfiStr,
    keys: *const FfiStr,
    keys_count: usize,
) -> i64 {
    ffi_guard(-1, || {
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
    })
}

extern "C" fn ss_clear_store(state: *mut c_void, store_id: FfiStr) -> i64 {
    ffi_guard(-1, || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        match block_on(provider.clear_store(&store_id)) {
            Some(Ok(count)) => count as i64,
            _ => -1,
        }
    })
}

extern "C" fn ss_list_keys(state: *mut c_void, store_id: FfiStr) -> FfiStringArray {
    ffi_guard(FfiStringArray::from_vec(Vec::new()), || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        match block_on(provider.list_keys(&store_id)) {
            Some(Ok(keys)) => FfiStringArray::from_vec(keys),
            _ => FfiStringArray::from_vec(Vec::new()),
        }
    })
}

extern "C" fn ss_store_exists(state: *mut c_void, store_id: FfiStr) -> FfiResult {
    ffi_guard(FfiResult::err("ss_store_exists: panic".to_string()), || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        match block_on(provider.store_exists(&store_id)) {
            Some(Ok(true)) => FfiResult::ok(),
            Some(Ok(false)) => FfiResult::err("not_found".to_string()),
            Some(Err(e)) => FfiResult::err(e.to_string()),
            None => FfiResult::err("failed to build runtime".to_string()),
        }
    })
}

extern "C" fn ss_key_count(state: *mut c_void, store_id: FfiStr) -> i64 {
    ffi_guard(-1, || {
        let provider = provider_ref(state);
        let store_id = unsafe { store_id.to_string() };
        match block_on(provider.key_count(&store_id)) {
            Some(Ok(count)) => count as i64,
            _ => -1,
        }
    })
}

extern "C" fn ss_sync(state: *mut c_void) -> FfiResult {
    ffi_guard(FfiResult::err("ss_sync: panic".to_string()), || {
        let provider = provider_ref(state);
        match block_on(provider.sync()) {
            Some(Ok(())) => FfiResult::ok(),
            Some(Err(e)) => FfiResult::err(e.to_string()),
            None => FfiResult::err("failed to build runtime".to_string()),
        }
    })
}

extern "C" fn ss_drop(state: *mut c_void) {
    ffi_guard((), || {
        // Reconstruct the Box<Arc<...>> and drop it
        unsafe { drop(Box::from_raw(state as *mut Arc<dyn StateStoreProvider>)) };
    })
}
