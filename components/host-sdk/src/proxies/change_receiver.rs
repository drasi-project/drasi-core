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

//! Host-side proxy for change receivers that wraps FFI vtables.

use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::channels::events::SourceEventWrapper;
use drasi_lib::channels::ChangeReceiver;
use drasi_plugin_sdk::ffi::{FfiChangeReceiver, FfiSourceEvent, SendMutPtr};

/// Shared FFI state that ensures the plugin-side receiver is not freed
/// while a `spawn_blocking` thread is still inside the FFI recv call.
struct SharedFfiState {
    recv_fn: extern "C" fn(*mut std::ffi::c_void) -> *mut FfiSourceEvent,
    drop_fn: extern "C" fn(*mut std::ffi::c_void),
    state: *mut std::ffi::c_void,
}

// Safety: The FFI function pointers and state are safe to call from any thread.
unsafe impl Send for SharedFfiState {}
unsafe impl Sync for SharedFfiState {}

impl Drop for SharedFfiState {
    fn drop(&mut self) {
        (self.drop_fn)(self.state);
    }
}

/// Wraps an `FfiChangeReceiver` into a `ChangeReceiver<SourceEventWrapper>`.
///
/// Uses `spawn_blocking` for FFI recv calls because the plugin-side recv
/// creates a nested tokio runtime, which corrupts the host runtime context
/// and breaks `tokio::sync::Notify` wakers if run on a worker thread.
pub struct ChangeReceiverProxy {
    shared: Arc<SharedFfiState>,
}

// Safety: FfiChangeReceiver is Send+Sync (vtable function pointers are safe to call from any thread)
unsafe impl Send for ChangeReceiverProxy {}
unsafe impl Sync for ChangeReceiverProxy {}

impl ChangeReceiverProxy {
    pub fn new(inner: FfiChangeReceiver) -> Self {
        Self {
            shared: Arc::new(SharedFfiState {
                recv_fn: inner.recv_fn,
                drop_fn: inner.drop_fn,
                state: inner.state,
            }),
        }
    }
}

#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for ChangeReceiverProxy {
    async fn recv(&mut self) -> anyhow::Result<Arc<SourceEventWrapper>> {
        // Clone the Arc so the FFI state stays alive even if this proxy is dropped
        // while the spawn_blocking thread is still blocked in the FFI recv call.
        let shared = Arc::clone(&self.shared);
        let result = tokio::task::spawn_blocking(move || {
            let ptr = (shared.recv_fn)(shared.state);
            SendMutPtr(ptr)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))?;
        let ptr = result.0;
        if ptr.is_null() {
            return Err(anyhow::anyhow!("Channel closed"));
        }
        let ffi_event = unsafe { &*ptr };
        let wrapper = unsafe { *Box::from_raw(ffi_event.opaque as *mut SourceEventWrapper) };
        unsafe { drop(Box::from_raw(ptr)) };
        Ok(Arc::new(wrapper))
    }
}

/// Wraps an `FfiBootstrapReceiver` as a tokio mpsc Receiver of `BootstrapEvent`.
///
/// Bootstrap events are finite — the receiver produces events until the bootstrap
/// is complete, then yields `None` (null pointer).
pub struct BootstrapReceiverProxy {
    inner: drasi_plugin_sdk::ffi::FfiBootstrapReceiver,
}

unsafe impl Send for BootstrapReceiverProxy {}
unsafe impl Sync for BootstrapReceiverProxy {}

impl BootstrapReceiverProxy {
    pub fn new(inner: drasi_plugin_sdk::ffi::FfiBootstrapReceiver) -> Self {
        Self { inner }
    }

    /// Receive the next bootstrap event, or None if the stream is complete.
    pub fn recv(&self) -> Option<drasi_lib::channels::events::BootstrapEvent> {
        let ptr = (self.inner.recv_fn)(self.inner.state);
        if ptr.is_null() {
            return None;
        }
        let ffi_event = unsafe { &*ptr };
        let event = unsafe {
            *Box::from_raw(ffi_event.opaque as *mut drasi_lib::channels::events::BootstrapEvent)
        };
        unsafe { drop(Box::from_raw(ptr)) };
        Some(event)
    }

    /// Convert into a tokio mpsc Receiver by spawning a forwarding task.
    pub fn into_mpsc_receiver(self) -> drasi_lib::channels::events::BootstrapEventReceiver {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        std::thread::spawn(move || {
            while let Some(event) = self.recv() {
                if tx.blocking_send(event).is_err() {
                    break;
                }
            }
        });
        rx
    }
}

impl Drop for BootstrapReceiverProxy {
    fn drop(&mut self) {
        (self.inner.drop_fn)(self.inner.state);
    }
}
