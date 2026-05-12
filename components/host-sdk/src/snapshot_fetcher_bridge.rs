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

//! Bridge from host-side `SnapshotFetcher` to FFI `SnapshotFetcherVtable`.
//!
//! The host creates a `SnapshotFetcherVtable` wrapping its real `SnapshotFetcher`
//! and passes it to plugins via `FfiRuntimeContext`. Plugins use the vtable
//! through `FfiSnapshotFetcherProxy` (in the plugin SDK) to fetch query snapshots.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_lib::SnapshotFetcher;
use drasi_plugin_sdk::ffi::{
    FfiOwnedStr, FfiSnapshotIterator, FfiSnapshotIteratorResponse, FfiStr, SnapshotFetcherVtable,
};

/// Wraps an FFI body in `catch_unwind` and returns `default` on panic.
fn ffi_guard<T, F: FnOnce() -> T>(default: T, f: F) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => default,
    }
}

/// Builds a `SnapshotFetcherVtable` from a host-side `Arc<dyn SnapshotFetcher>`.
pub struct SnapshotFetcherVtableBuilder;

impl SnapshotFetcherVtableBuilder {
    /// Build a `SnapshotFetcherVtable` that dispatches to the given `SnapshotFetcher`.
    ///
    /// The returned vtable holds an `Arc` clone — the fetcher stays alive as long
    /// as the vtable (or any plugin holding it) is alive.
    pub fn build(fetcher: Arc<dyn SnapshotFetcher>) -> SnapshotFetcherVtable {
        let boxed = Box::new(fetcher);
        let state = Box::into_raw(boxed) as *mut c_void;
        SnapshotFetcherVtable {
            state,
            fetch_snapshot_fn: sf_fetch_snapshot,
            drop_fn: sf_drop,
        }
    }
}

fn fetcher_ref(state: *mut c_void) -> &'static dyn SnapshotFetcher {
    let arc = unsafe { &*(state as *const Arc<dyn SnapshotFetcher>) };
    arc.as_ref()
}

fn block_on<F: std::future::Future>(f: F) -> Option<F::Output> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    Some(rt.block_on(f))
}

/// Host-allocated snapshot iterator state.
struct SnapshotIteratorState {
    rows: Vec<serde_json::Value>,
    pos: usize,
}

extern "C" fn snapshot_iter_next(iter_ctx: *mut c_void) -> FfiOwnedStr {
    ffi_guard(FfiOwnedStr::from_string(String::new()), || {
        if iter_ctx.is_null() {
            return FfiOwnedStr::from_string(String::new());
        }
        let state = unsafe { &mut *(iter_ctx as *mut SnapshotIteratorState) };
        if state.pos >= state.rows.len() {
            return FfiOwnedStr::from_string(String::new());
        }
        let row = &state.rows[state.pos];
        state.pos += 1;
        let json = serde_json::to_string(row).unwrap_or_else(|_| "null".into());
        FfiOwnedStr::from_string(json)
    })
}

extern "C" fn snapshot_iter_drop(iter_ctx: *mut c_void) {
    ffi_guard((), || {
        if !iter_ctx.is_null() {
            unsafe {
                drop(Box::from_raw(iter_ctx as *mut SnapshotIteratorState));
            }
        }
    })
}

fn make_error_response(msg: String) -> FfiSnapshotIteratorResponse {
    FfiSnapshotIteratorResponse {
        iterator: FfiSnapshotIterator {
            iter_ctx: std::ptr::null_mut(),
            next_fn: snapshot_iter_next,
            drop_fn: snapshot_iter_drop,
        },
        as_of_sequence: 0,
        config_hash: 0,
        error: FfiOwnedStr::from_string(msg),
    }
}

extern "C" fn sf_fetch_snapshot(
    state: *mut c_void,
    query_id: FfiStr,
) -> FfiSnapshotIteratorResponse {
    if state.is_null() {
        return make_error_response("null snapshot fetcher state".into());
    }
    ffi_guard(
        make_error_response("panic in snapshot fetcher callback".into()),
        || {
            let fetcher = fetcher_ref(state);
            let query_id_str = unsafe { query_id.to_string() };

            match block_on(async {
                let snapshot = fetcher.fetch_snapshot(&query_id_str).await?;
                let as_of_sequence = snapshot.as_of_sequence;
                let config_hash = snapshot.config_hash;
                let rows = snapshot.collect_vec().await;
                Ok::<_, drasi_lib::queries::output_state::FetchError>((
                    rows,
                    as_of_sequence,
                    config_hash,
                ))
            }) {
                Some(Ok((rows, as_of_sequence, config_hash))) => {
                    let iter_state = Box::new(SnapshotIteratorState { rows, pos: 0 });
                    let iter_ctx = Box::into_raw(iter_state) as *mut c_void;

                    FfiSnapshotIteratorResponse {
                        iterator: FfiSnapshotIterator {
                            iter_ctx,
                            next_fn: snapshot_iter_next,
                            drop_fn: snapshot_iter_drop,
                        },
                        as_of_sequence,
                        config_hash,
                        error: FfiOwnedStr::from_string(String::new()),
                    }
                }
                Some(Err(e)) => make_error_response(format!("{e}")),
                None => make_error_response("failed to build runtime for snapshot fetch".into()),
            }
        },
    )
}

extern "C" fn sf_drop(state: *mut c_void) {
    ffi_guard((), || {
        if !state.is_null() {
            unsafe { drop(Box::from_raw(state as *mut Arc<dyn SnapshotFetcher>)) };
        }
    })
}
