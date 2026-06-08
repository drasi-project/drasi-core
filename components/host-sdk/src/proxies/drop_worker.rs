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

//! A shared long-lived worker thread for executing plugin `drop_fn` calls.
//!
//! On macOS arm64, spawning a short-lived thread for each `drop_fn` call and
//! immediately joining it causes TLS (Thread-Local Storage) destructors to run
//! when the thread exits. When multiple such threads exit near-simultaneously
//! during teardown, these destructors race with the plugin's still-active tokio
//! runtime, causing SIGSEGV.
//!
//! This module provides a single persistent worker thread that processes all
//! `drop_fn` calls sequentially. Because the thread is long-lived, its TLS is
//! never torn down between drops, eliminating the race.
//!
//! ## Tradeoffs
//!
//! All drops are serialised through one thread, so a single slow or hung
//! `drop_fn` blocks every drop queued behind it (the previous per-drop-thread
//! approach isolated a hang to its own caller). This is an acceptable tradeoff:
//! plugin `drop_fn`s are expected to be fast, non-blocking teardown calls, and
//! avoiding the TLS-destructor SIGSEGV is the priority. Callers should not
//! perform long-running or blocking work inside a plugin `drop_fn`.
//!
//! ## Re-entrancy
//!
//! If a `drop_fn` running on the worker thread transitively triggers another
//! Drasi proxy `Drop` (which calls [`execute_drop_fn`] again), enqueuing the
//! nested request and blocking on it would deadlock — the worker cannot process
//! the new request while it is still executing the outer `drop_fn`. A
//! thread-local guard detects this case and runs the nested drop inline instead.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::OnceLock;

thread_local! {
    /// Set while the drop worker thread is executing a `drop_fn`. Used by
    /// [`execute_drop_fn`] to detect re-entrant calls and run them inline,
    /// avoiding a self-deadlock on the worker thread.
    static ON_DROP_WORKER: Cell<bool> = const { Cell::new(false) };
}

/// A request to execute a `drop_fn` on the worker thread.
///
/// This is automatically `Send` because all of its fields are `Send`: the
/// `extern "C" fn` pointer, `SendMutPtr<c_void>` (which carries its own
/// `unsafe impl Send`), and the `SyncSender`. Adding a manual `unsafe impl Send`
/// here would be redundant and would suppress the compiler's check if a
/// non-`Send` field were ever added.
struct DropRequest {
    drop_fn: extern "C" fn(*mut c_void),
    state: drasi_plugin_sdk::ffi::SendMutPtr<c_void>,
    /// Sender to notify the caller that the drop has completed.
    done_tx: mpsc::SyncSender<()>,
}

/// Channel sender for the global drop worker.
static DROP_WORKER: OnceLock<mpsc::Sender<DropRequest>> = OnceLock::new();

/// Get or initialize the global drop worker channel.
fn drop_worker_tx() -> &'static mpsc::Sender<DropRequest> {
    DROP_WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<DropRequest>();
        std::thread::Builder::new()
            .name("drasi-drop-worker".to_string())
            .spawn(move || {
                for request in rx {
                    ON_DROP_WORKER.with(|f| f.set(true));
                    (request.drop_fn)(request.state.as_ptr());
                    ON_DROP_WORKER.with(|f| f.set(false));
                    let _ = request.done_tx.send(());
                }
            })
            .expect("Failed to spawn drasi-drop-worker thread: system may be out of resources");
        tx
    })
}

/// Execute a plugin `drop_fn` on the shared worker thread and block until
/// it completes. This avoids spawning a short-lived thread whose TLS
/// destructors could race with the plugin runtime on macOS arm64.
pub(crate) fn execute_drop_fn(
    drop_fn: extern "C" fn(*mut c_void),
    state: drasi_plugin_sdk::ffi::SendMutPtr<c_void>,
) {
    // Re-entrancy guard: if we are already executing on the worker thread (a
    // `drop_fn` transitively triggered another proxy drop), enqueuing and
    // blocking would deadlock because the worker is busy with the outer call.
    // Run the nested drop inline instead — it shares the worker's long-lived
    // TLS, so it does not reintroduce the destructor race.
    if ON_DROP_WORKER.with(|f| f.get()) {
        (drop_fn)(state.as_ptr());
        return;
    }

    // Save the raw pointer before moving `state` into the request, in case
    // the worker channel is disconnected and we need the fallback path.
    let raw_ptr = state.as_ptr();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let request = DropRequest {
        drop_fn,
        state,
        done_tx,
    };

    // If the worker thread has been shut down (channel disconnected), fall back
    // to a dedicated thread spawn. This should not happen in normal operation.
    if drop_worker_tx().send(request).is_ok() {
        // A `RecvError` means the worker exited before signalling completion
        // (e.g. it panicked or was terminated). `extern "C"` unwinding aborts
        // the process, so this is not expected — but if it ever happens, emit a
        // diagnostic rather than silently proceeding as if the drop completed.
        if done_rx.recv().is_err() {
            eprintln!(
                "[drasi-drop-worker] worker exited before completing drop_fn; \
                 plugin resources may have leaked"
            );
        }
    } else {
        let fallback_state = drasi_plugin_sdk::ffi::SendMutPtr(raw_ptr);
        let _ = std::thread::spawn(move || (drop_fn)(fallback_state.as_ptr())).join();
    }
}
