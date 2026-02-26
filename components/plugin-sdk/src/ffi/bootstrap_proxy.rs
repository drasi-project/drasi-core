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

//! Plugin-side bootstrap provider proxy.
//!
//! Wraps a `BootstrapProviderVtable` (from another cdylib plugin or the host)
//! into a local `BootstrapProvider` trait implementation. This is how a source
//! plugin calls a bootstrap provider that lives in a different cdylib.

use std::ffi::c_void;
use std::sync::Mutex;

use super::types::{FfiStr, now_us};
use super::vtables::{BootstrapProviderVtable, FfiBootstrapEvent, FfiBootstrapSender};
use drasi_lib::bootstrap::BootstrapProvider;

/// Plugin-side proxy: wraps a `BootstrapProviderVtable` into a local `BootstrapProvider`.
pub struct FfiBootstrapProviderProxy {
    pub(crate) vtable: Mutex<BootstrapProviderVtable>,
}

unsafe impl Send for FfiBootstrapProviderProxy {}
unsafe impl Sync for FfiBootstrapProviderProxy {}

#[async_trait::async_trait]
impl BootstrapProvider for FfiBootstrapProviderProxy {
    async fn bootstrap(
        &self,
        request: drasi_lib::bootstrap::BootstrapRequest,
        context: &drasi_lib::bootstrap::BootstrapContext,
        event_tx: drasi_lib::channels::events::BootstrapEventSender,
        settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<usize> {
        // Extract vtable fields under the lock, then release immediately
        let (vtable_state, vtable_bootstrap_fn) = {
            let vtable = self.vtable.lock().unwrap();
            (vtable.state, vtable.bootstrap_fn)
        };

        let query_id = request.query_id.clone();
        let node_labels: Vec<String> = request.node_labels.clone();
        let relation_labels: Vec<String> = request.relation_labels.clone();
        let request_id = request.request_id.clone();
        let server_id = context.server_id.clone();
        let source_id = context.source_id.clone();

        // FFI sender callback — wraps tokio sender, called from the bootstrap thread
        struct SenderState {
            tx: std::sync::mpsc::Sender<drasi_lib::channels::events::BootstrapEvent>,
        }

        extern "C" fn send_fn(state: *mut c_void, event: *mut FfiBootstrapEvent) -> i32 {
            let sender_state = unsafe { &*(state as *const SenderState) };
            if event.is_null() {
                return -1;
            }
            let ffi_event = unsafe { &*event };
            let bootstrap_event = unsafe {
                let opaque = ffi_event.opaque as *mut drasi_lib::channels::events::BootstrapEvent;
                std::ptr::read(opaque)
            };
            match sender_state.tx.send(bootstrap_event) {
                Ok(()) => 0,
                Err(_) => -1,
            }
        }

        extern "C" fn drop_sender(state: *mut c_void) {
            unsafe { drop(Box::from_raw(state as *mut SenderState)) };
        }

        // Use std::sync::mpsc so the vtable thread can send without async
        let (std_tx, std_rx) = std::sync::mpsc::channel::<drasi_lib::channels::events::BootstrapEvent>();

        let sender_state = Box::new(SenderState { tx: std_tx });
        let ffi_sender = Box::new(FfiBootstrapSender {
            state: Box::into_raw(sender_state) as *mut c_void,
            send_fn,
            drop_fn: drop_sender,
        });

        // Spawn the vtable call on a background thread (breaks out of source's tokio).
        let vtable_state_usize = vtable_state as usize;
        let bootstrap_fn = vtable_bootstrap_fn;

        std::thread::spawn(move || {
            let ffi_query_id = FfiStr::from_str(&query_id);
            let ffi_node_labels: Vec<FfiStr> =
                node_labels.iter().map(|s| FfiStr::from_str(s)).collect();
            let ffi_rel_labels: Vec<FfiStr> =
                relation_labels.iter().map(|s| FfiStr::from_str(s)).collect();
            let ffi_request_id = FfiStr::from_str(&request_id);
            let ffi_server_id = FfiStr::from_str(&server_id);
            let ffi_source_id = FfiStr::from_str(&source_id);

            let ffi_sender_ptr = Box::into_raw(ffi_sender);
            (bootstrap_fn)(
                vtable_state_usize as *mut c_void,
                ffi_query_id,
                ffi_node_labels.as_ptr(),
                ffi_node_labels.len(),
                ffi_rel_labels.as_ptr(),
                ffi_rel_labels.len(),
                ffi_request_id,
                ffi_server_id,
                ffi_source_id,
                ffi_sender_ptr,
            );
            // Properly drop the FfiBootstrapSender: call drop_fn to free inner state
            let ffi_sender = unsafe { Box::from_raw(ffi_sender_ptr) };
            (ffi_sender.drop_fn)(ffi_sender.state);
        });

        // Forward from std::sync::mpsc → tokio::sync::mpsc on a blocking thread.
        let count = tokio::task::spawn_blocking(move || {
            let mut count: usize = 0;
            while let Ok(record) = std_rx.recv() {
                if event_tx.blocking_send(record).is_err() {
                    break;
                }
                count += 1;
            }
            count
        })
        .await
        .expect("forwarding task panicked");

        Ok(count)
    }
}
