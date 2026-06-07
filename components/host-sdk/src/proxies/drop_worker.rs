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

use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::OnceLock;

/// A request to execute a `drop_fn` on the worker thread.
struct DropRequest {
    drop_fn: extern "C" fn(*mut c_void),
    state: drasi_plugin_sdk::ffi::SendMutPtr<c_void>,
    /// Sender to notify the caller that the drop has completed.
    done_tx: mpsc::SyncSender<()>,
}

// Safety: DropRequest is sent between threads; the contained function pointer
// and SendMutPtr are designed for cross-thread use.
unsafe impl Send for DropRequest {}

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
                    (request.drop_fn)(request.state.as_ptr());
                    let _ = request.done_tx.send(());
                }
            })
            .expect("Failed to spawn drasi-drop-worker thread");
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
    let ptr = state.as_ptr();
    let (done_tx, done_rx) = mpsc::sync_channel(0);
    let request = DropRequest {
        drop_fn,
        state,
        done_tx,
    };

    // If the worker thread has been shut down (channel disconnected), fall back
    // to a dedicated thread spawn. This should not happen in normal operation.
    if drop_worker_tx().send(request).is_ok() {
        let _ = done_rx.recv();
    } else {
        let fallback_state = drasi_plugin_sdk::ffi::SendMutPtr(ptr);
        let _ = std::thread::spawn(move || (drop_fn)(fallback_state.as_ptr())).join();
    }
}
