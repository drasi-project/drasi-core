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
use drasi_plugin_sdk::ffi::FfiChangeReceiver;

/// Wraps an `FfiChangeReceiver` into a `ChangeReceiver<SourceEventWrapper>`.
///
/// The FFI receiver delivers `FfiSourceEvent` with opaque pointers to real
/// `SourceEventWrapper` values. This proxy extracts the inner Rust type
/// and returns it through the standard DrasiLib trait.
pub struct ChangeReceiverProxy {
    inner: FfiChangeReceiver,
}

// Safety: FfiChangeReceiver is Send+Sync (vtable function pointers are safe to call from any thread)
unsafe impl Send for ChangeReceiverProxy {}
unsafe impl Sync for ChangeReceiverProxy {}

impl ChangeReceiverProxy {
    pub fn new(inner: FfiChangeReceiver) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for ChangeReceiverProxy {
    async fn recv(&mut self) -> anyhow::Result<Arc<SourceEventWrapper>> {
        let ptr = (self.inner.recv_fn)(self.inner.state);
        if ptr.is_null() {
            return Err(anyhow::anyhow!("Channel closed"));
        }
        let ffi_event = unsafe { &*ptr };
        // The opaque pointer holds a Box<SourceEventWrapper> created by the plugin SDK.
        // We take ownership by reconstructing the Box, then wrap in Arc.
        let wrapper = unsafe { *Box::from_raw(ffi_event.opaque as *mut SourceEventWrapper) };
        // Drop the FFI envelope (but NOT the opaque — we already took ownership)
        // We need to free the FfiSourceEvent box itself without calling drop_fn on the opaque
        unsafe { drop(Box::from_raw(ptr)) };
        Ok(Arc::new(wrapper))
    }
}

impl Drop for ChangeReceiverProxy {
    fn drop(&mut self) {
        (self.inner.drop_fn)(self.inner.state);
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
