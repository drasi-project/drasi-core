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
//!
//! The iterator returned by `fetch_snapshot_fn` streams rows lazily: each
//! `next_fn` call pulls exactly one row from the underlying `SnapshotStream`
//! via a dedicated current-thread tokio runtime stored alongside the stream.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_lib::queries::output_state::SnapshotStream;
use drasi_lib::SnapshotFetcher;
use drasi_plugin_sdk::ffi::{
    FfiOwnedStr, FfiSnapshotIterator, FfiSnapshotIteratorResponse, FfiStr, SnapshotFetcherVtable,
};
use tokio_stream::StreamExt;

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

/// Host-allocated snapshot iterator state that streams rows lazily.
///
/// Holds a dedicated current-thread tokio runtime and the `SnapshotStream`.
/// Each `next_fn` call pulls exactly one row via `rt.block_on(stream.next())`,
/// avoiding eager collection into memory.
///
/// # Safety
///
/// The FFI contract guarantees sequential access: the plugin calls `next_fn`
/// from a single `async_stream` loop (via `spawn_blocking`) and calls `drop_fn`
/// exactly once after iteration completes. Concurrent calls are undefined behavior.
///
/// `next_fn` uses `Runtime::block_on`, which requires that the caller is NOT
/// inside an active tokio runtime context. This holds because `next_fn` is
/// dispatched via `spawn_blocking` on the plugin side.
struct SnapshotIteratorState {
    /// `Option` so `drop_fn` can move it to a dedicated thread for safe cleanup.
    rt: Option<tokio::runtime::Runtime>,
    /// `Option` so we can drop the stream before the runtime in `drop_fn`.
    stream: Option<SnapshotStream>,
}

extern "C" fn snapshot_iter_next(iter_ctx: *mut c_void) -> FfiOwnedStr {
    ffi_guard(FfiOwnedStr::from_string(String::new()), || {
        if iter_ctx.is_null() {
            return FfiOwnedStr::from_string(String::new());
        }
        let state = unsafe { &mut *(iter_ctx as *mut SnapshotIteratorState) };
        let (rt, stream) = match (state.rt.as_ref(), state.stream.as_mut()) {
            (Some(rt), Some(s)) => (rt, s),
            _ => return FfiOwnedStr::from_string(String::new()),
        };
        match rt.block_on(stream.next()) {
            Some(row) => {
                let json = serde_json::to_string(&row).unwrap_or_else(|_| "null".into());
                FfiOwnedStr::from_string(json)
            }
            None => FfiOwnedStr::from_string(String::new()),
        }
    })
}

extern "C" fn snapshot_iter_drop(iter_ctx: *mut c_void) {
    ffi_guard((), || {
        if !iter_ctx.is_null() {
            let mut state = unsafe { Box::from_raw(iter_ctx as *mut SnapshotIteratorState) };
            // Drop the stream first to release any async resources.
            state.stream = None;
            // The runtime cannot be dropped from within a tokio async context
            // (e.g. when drop_fn is called on a worker thread). Move it to a
            // dedicated thread where dropping is always safe.
            if let Some(rt) = state.rt.take() {
                std::thread::spawn(move || drop(rt));
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

            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    return make_error_response(format!(
                        "failed to build runtime for snapshot fetch: {e}"
                    ));
                }
            };

            let snapshot = match rt.block_on(fetcher.fetch_snapshot(&query_id_str)) {
                Ok(s) => s,
                Err(e) => return make_error_response(format!("{e}")),
            };

            let as_of_sequence = snapshot.as_of_sequence;
            let config_hash = snapshot.config_hash;

            let iter_state = Box::new(SnapshotIteratorState {
                rt: Some(rt),
                stream: Some(snapshot),
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use drasi_lib::queries::output_state::FetchError;
    use drasi_lib::ComponentStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `SnapshotFetcher` that returns a stream tracking per-row consumption.
    struct TrackingSnapshotFetcher {
        rows: Vec<serde_json::Value>,
        consumed_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SnapshotFetcher for TrackingSnapshotFetcher {
        async fn fetch_snapshot(&self, _query_id: &str) -> Result<SnapshotStream, FetchError> {
            let consumed = self.consumed_count.clone();
            let items = self.rows.clone();

            let stream = async_stream::stream! {
                for item in items {
                    consumed.fetch_add(1, Ordering::SeqCst);
                    yield item;
                }
            };

            Ok(SnapshotStream::from_stream(stream, 42, 12345))
        }
    }

    /// A `SnapshotFetcher` that always returns an error.
    struct FailingSnapshotFetcher;

    #[async_trait]
    impl SnapshotFetcher for FailingSnapshotFetcher {
        async fn fetch_snapshot(&self, _query_id: &str) -> Result<SnapshotStream, FetchError> {
            Err(FetchError::NotRunning {
                status: ComponentStatus::Error,
            })
        }
    }

    /// Helper: call `next_fn` and return the deserialized JSON, or `None` on EOF.
    unsafe fn pull_next(iter: &FfiSnapshotIterator) -> Option<serde_json::Value> {
        let owned = (iter.next_fn)(iter.iter_ctx);
        let json_str = owned.into_string();
        if json_str.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&json_str).expect("valid JSON"))
        }
    }

    #[test]
    fn streaming_bridge_pulls_rows_lazily() {
        let consumed = Arc::new(AtomicUsize::new(0));
        let rows = vec![
            serde_json::json!({"name": "Alice"}),
            serde_json::json!({"name": "Bob"}),
            serde_json::json!({"name": "Charlie"}),
        ];

        let fetcher = TrackingSnapshotFetcher {
            rows: rows.clone(),
            consumed_count: consumed.clone(),
        };
        let vtable = SnapshotFetcherVtableBuilder::build(Arc::new(fetcher));

        // Fetch the iterator.
        let query_id = "test-query\0";
        let response = (vtable.fetch_snapshot_fn)(vtable.state, FfiStr::from_str(query_id));
        let err = unsafe { response.error.into_string() };
        assert!(err.is_empty(), "unexpected error: {err}");
        assert_eq!(response.as_of_sequence, 42);
        assert_eq!(response.config_hash, 12345);

        // No rows consumed yet — stream is lazy.
        assert_eq!(consumed.load(Ordering::SeqCst), 0);

        // Pull rows one at a time and verify lazy consumption.
        for (i, expected) in rows.iter().enumerate() {
            let val = unsafe { pull_next(&response.iterator) }.expect("expected a row");
            assert_eq!(&val, expected);
            assert_eq!(consumed.load(Ordering::SeqCst), i + 1);
        }

        // Stream exhausted — next returns None.
        assert!(unsafe { pull_next(&response.iterator) }.is_none());

        // Cleanup.
        (response.iterator.drop_fn)(response.iterator.iter_ctx);
        (vtable.drop_fn)(vtable.state);
    }

    #[test]
    fn streaming_bridge_drop_before_exhaustion() {
        let consumed = Arc::new(AtomicUsize::new(0));
        let rows = vec![
            serde_json::json!({"a": 1}),
            serde_json::json!({"a": 2}),
            serde_json::json!({"a": 3}),
        ];

        let fetcher = TrackingSnapshotFetcher {
            rows,
            consumed_count: consumed.clone(),
        };
        let vtable = SnapshotFetcherVtableBuilder::build(Arc::new(fetcher));

        let response = (vtable.fetch_snapshot_fn)(vtable.state, FfiStr::from_str("q\0"));
        let err = unsafe { response.error.into_string() };
        assert!(err.is_empty());

        // Pull only the first row.
        let val = unsafe { pull_next(&response.iterator) }.expect("expected a row");
        assert_eq!(val, serde_json::json!({"a": 1}));
        assert_eq!(consumed.load(Ordering::SeqCst), 1);

        // Drop before exhaustion — remaining rows should NOT be consumed.
        (response.iterator.drop_fn)(response.iterator.iter_ctx);
        assert_eq!(consumed.load(Ordering::SeqCst), 1);

        (vtable.drop_fn)(vtable.state);
    }

    #[test]
    fn streaming_bridge_propagates_fetch_error() {
        let vtable = SnapshotFetcherVtableBuilder::build(Arc::new(FailingSnapshotFetcher));

        let response = (vtable.fetch_snapshot_fn)(vtable.state, FfiStr::from_str("q\0"));
        let err = unsafe { response.error.into_string() };
        assert!(!err.is_empty(), "expected an error message");

        // Iterator ctx should be null on error — drop is safe.
        assert!(response.iterator.iter_ctx.is_null());
        (response.iterator.drop_fn)(response.iterator.iter_ctx);

        (vtable.drop_fn)(vtable.state);
    }

    #[test]
    fn streaming_bridge_empty_snapshot() {
        let consumed = Arc::new(AtomicUsize::new(0));
        let fetcher = TrackingSnapshotFetcher {
            rows: vec![],
            consumed_count: consumed.clone(),
        };
        let vtable = SnapshotFetcherVtableBuilder::build(Arc::new(fetcher));

        let response = (vtable.fetch_snapshot_fn)(vtable.state, FfiStr::from_str("q\0"));
        let err = unsafe { response.error.into_string() };
        assert!(err.is_empty());

        // Immediately exhausted.
        assert!(unsafe { pull_next(&response.iterator) }.is_none());
        assert_eq!(consumed.load(Ordering::SeqCst), 0);

        (response.iterator.drop_fn)(response.iterator.iter_ctx);
        (vtable.drop_fn)(vtable.state);
    }
}
