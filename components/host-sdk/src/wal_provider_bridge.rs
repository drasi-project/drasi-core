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

//! Bridge from host-side `WalProvider` to FFI `WalProviderVtable`.
//!
//! The host creates a `WalProviderVtable` wrapping its real `WalProvider`
//! and passes it to plugins via `FfiRuntimeContext`. Plugins use the vtable
//! through `FfiWalProviderProxy` (in the plugin SDK) to access the WAL.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_core::models::SourceChange;
use drasi_ffi_primitives::{
    FfiWalAppendResult, FfiWalEntry, FfiWalOptionalU64Result, FfiWalReadResult, FfiWalU64Result,
};
use drasi_lib::wal::{CapacityPolicy, WalProvider, WriteAheadLogConfig};
use drasi_plugin_sdk::ffi::vtables::WalProviderVtable;
use drasi_plugin_sdk::ffi::{FfiResult, FfiStr};

/// Wraps an FFI body in `catch_unwind` and returns `default` on panic.
fn ffi_guard<T, F: FnOnce() -> T>(default: T, f: F) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => default,
    }
}

/// Combined state stored behind the vtable's opaque `state` pointer.
/// Holds both the provider and the runtime handle captured at build time,
/// so FFI functions can reuse the host's Tokio runtime instead of creating
/// a new one per call.
struct WalBridgeState {
    provider: Arc<dyn WalProvider>,
    runtime: tokio::runtime::Handle,
}

/// Builds a `WalProviderVtable` from a host-side `Arc<dyn WalProvider>`.
pub struct WalProviderVtableBuilder;

impl WalProviderVtableBuilder {
    /// Build a `WalProviderVtable` that dispatches to the given `WalProvider`.
    ///
    /// Captures the current Tokio runtime handle so FFI callbacks can reuse it.
    /// Must be called from within an active Tokio runtime context.
    pub fn build(provider: Arc<dyn WalProvider>) -> WalProviderVtable {
        let state = WalBridgeState {
            provider,
            runtime: tokio::runtime::Handle::current(),
        };
        let boxed = Box::new(state);
        let state_ptr = Box::into_raw(boxed) as *mut c_void;
        WalProviderVtable {
            state: state_ptr,
            register_fn: wal_register,
            append_fn: wal_append,
            read_from_fn: wal_read_from,
            prune_up_to_fn: wal_prune_up_to,
            head_sequence_fn: wal_head_sequence,
            oldest_sequence_fn: wal_oldest_sequence,
            event_count_fn: wal_event_count,
            delete_wal_fn: wal_delete_wal,
            free_read_result_fn: wal_free_read_result,
            drop_fn: wal_drop,
        }
    }
}

/// Clone the provider Arc from the opaque state pointer.
/// Each FFI call gets its own strong reference, preventing use-after-free
/// if `drop_fn` is called concurrently.
fn clone_provider(state: *mut c_void) -> (Arc<dyn WalProvider>, tokio::runtime::Handle) {
    let bridge_state = unsafe { &*(state as *const WalBridgeState) };
    (bridge_state.provider.clone(), bridge_state.runtime.clone())
}

extern "C" fn wal_register(
    state: *mut c_void,
    source_id: FfiStr,
    max_events: u64,
    capacity_policy: u8,
) -> FfiResult {
    ffi_guard(FfiResult::err("wal_register: panic".to_string()), || {
        let (provider, rt) = clone_provider(state);
        let source_id = unsafe { source_id.to_string() };
        let policy = match capacity_policy {
            0 => CapacityPolicy::RejectIncoming,
            1 => CapacityPolicy::OverwriteOldest,
            _ => return FfiResult::err(format!("invalid capacity_policy: {capacity_policy}")),
        };
        let config = WriteAheadLogConfig {
            max_events,
            capacity_policy: policy,
        };
        match rt.block_on(provider.register(&source_id, config)) {
            Ok(()) => FfiResult::ok(),
            Err(e) => FfiResult::err(e.to_string()),
        }
    })
}

extern "C" fn wal_append(
    state: *mut c_void,
    source_id: FfiStr,
    event: *const c_void,
) -> FfiWalAppendResult {
    ffi_guard(
        FfiWalAppendResult::err("wal_append: panic".to_string()),
        || {
            if event.is_null() {
                return FfiWalAppendResult::err("wal_append: null event pointer".to_string());
            }
            let (provider, rt) = clone_provider(state);
            let source_id = unsafe { source_id.to_string() };
            // Safety: the plugin passes a valid *const SourceChange that it owns.
            // We only borrow it for the duration of this call.
            let source_change = unsafe { &*(event as *const SourceChange) };
            match rt.block_on(provider.append(&source_id, source_change)) {
                Ok(seq) => FfiWalAppendResult::ok(seq),
                Err(drasi_lib::wal::WalError::CapacityExhausted(_)) => {
                    FfiWalAppendResult::capacity_exhausted(&source_id)
                }
                Err(e) => FfiWalAppendResult::err(e.to_string()),
            }
        },
    )
}

extern "C" fn wal_read_from(
    state: *mut c_void,
    source_id: FfiStr,
    sequence: u64,
) -> FfiWalReadResult {
    ffi_guard(
        FfiWalReadResult::err("wal_read_from: panic".to_string()),
        || {
            let (provider, rt) = clone_provider(state);
            let source_id = unsafe { source_id.to_string() };
            match rt.block_on(provider.read_from(&source_id, sequence)) {
                Ok(events) => {
                    if events.is_empty() {
                        return FfiWalReadResult::empty();
                    }
                    let ffi_entries: Vec<FfiWalEntry> = events
                        .into_iter()
                        .map(|(seq, change)| {
                            // Heap-allocate the SourceChange; plugin takes ownership
                            let boxed = Box::new(change);
                            FfiWalEntry {
                                sequence: seq,
                                event: Box::into_raw(boxed) as *mut c_void,
                            }
                        })
                        .collect();
                    FfiWalReadResult::ok(ffi_entries)
                }
                Err(e) => FfiWalReadResult::err(e.to_string()),
            }
        },
    )
}

extern "C" fn wal_prune_up_to(
    state: *mut c_void,
    source_id: FfiStr,
    sequence: u64,
) -> FfiWalU64Result {
    ffi_guard(
        FfiWalU64Result::err("wal_prune_up_to: panic".to_string()),
        || {
            let (provider, rt) = clone_provider(state);
            let source_id = unsafe { source_id.to_string() };
            match rt.block_on(provider.prune_up_to(&source_id, sequence)) {
                Ok(count) => FfiWalU64Result::ok(count),
                Err(e) => FfiWalU64Result::err(e.to_string()),
            }
        },
    )
}

extern "C" fn wal_head_sequence(state: *mut c_void, source_id: FfiStr) -> FfiWalU64Result {
    ffi_guard(
        FfiWalU64Result::err("wal_head_sequence: panic".to_string()),
        || {
            let (provider, rt) = clone_provider(state);
            let source_id = unsafe { source_id.to_string() };
            match rt.block_on(provider.head_sequence(&source_id)) {
                Ok(seq) => FfiWalU64Result::ok(seq),
                Err(e) => FfiWalU64Result::err(e.to_string()),
            }
        },
    )
}

extern "C" fn wal_oldest_sequence(
    state: *mut c_void,
    source_id: FfiStr,
) -> FfiWalOptionalU64Result {
    ffi_guard(
        FfiWalOptionalU64Result::err("wal_oldest_sequence: panic".to_string()),
        || {
            let (provider, rt) = clone_provider(state);
            let source_id = unsafe { source_id.to_string() };
            match rt.block_on(provider.oldest_sequence(&source_id)) {
                Ok(Some(seq)) => FfiWalOptionalU64Result::some(seq),
                Ok(None) => FfiWalOptionalU64Result::none(),
                Err(e) => FfiWalOptionalU64Result::err(e.to_string()),
            }
        },
    )
}

extern "C" fn wal_event_count(state: *mut c_void, source_id: FfiStr) -> FfiWalU64Result {
    ffi_guard(
        FfiWalU64Result::err("wal_event_count: panic".to_string()),
        || {
            let (provider, rt) = clone_provider(state);
            let source_id = unsafe { source_id.to_string() };
            match rt.block_on(provider.event_count(&source_id)) {
                Ok(count) => FfiWalU64Result::ok(count),
                Err(e) => FfiWalU64Result::err(e.to_string()),
            }
        },
    )
}

extern "C" fn wal_delete_wal(state: *mut c_void, source_id: FfiStr) -> FfiResult {
    ffi_guard(FfiResult::err("wal_delete_wal: panic".to_string()), || {
        let (provider, rt) = clone_provider(state);
        let source_id = unsafe { source_id.to_string() };
        match rt.block_on(provider.delete_wal(&source_id)) {
            Ok(()) => FfiResult::ok(),
            Err(e) => FfiResult::err(e.to_string()),
        }
    })
}

extern "C" fn wal_free_read_result(entries: *mut FfiWalEntry, count: usize, capacity: usize) {
    if entries.is_null() || count == 0 {
        return;
    }
    // Reconstruct the Vec with correct capacity to free the buffer memory.
    // Note: the SourceChange pointers within entries have already been consumed
    // by the plugin (Box::from_raw), so we only free the entries array itself.
    unsafe {
        let _ = Vec::from_raw_parts(entries, count, capacity);
    }
}

extern "C" fn wal_drop(state: *mut c_void) {
    if !state.is_null() {
        unsafe {
            let _ = Box::from_raw(state as *mut WalBridgeState);
        }
    }
}
