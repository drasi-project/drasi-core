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
//!
//! Uses a **push-based** model: the plugin spawns a forwarder task on its
//! own runtime that reads from the underlying channel and pushes events
//! via a callback into a host-side channel. The proxy's `recv()` simply
//! reads from that channel — pure async with no FFI overhead per event.
//!
//! The cross-boundary channel uses `std::sync::mpsc` (not `tokio::sync::mpsc`)
//! because the callback runs on the plugin's tokio runtime, which is a
//! separate cdylib copy of tokio with incompatible internal state.

use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::channels::events::SourceEventWrapper;
use drasi_lib::channels::ChangeReceiver;
use drasi_plugin_sdk::ffi::{FfiChangeReceiver, FfiSourceEvent};

/// Context passed to the push callback. Holds the std mpsc sender and a
/// tokio Notify to wake the host receiver.
struct PushCallbackContext {
    tx: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<Arc<SourceEventWrapper>>>>,
    notify: Arc<tokio::sync::Notify>,
}

/// The push callback invoked by the plugin forwarder for each event.
/// Uses `std::sync::mpsc` which is safe to call from any runtime.
extern "C" fn change_push_callback(
    ctx: *mut std::ffi::c_void,
    event: *mut FfiSourceEvent,
) -> bool {
    let context = unsafe { &*(ctx as *const PushCallbackContext) };

    if event.is_null() {
        // Channel closed — drop the sender to signal the host receiver
        let mut guard = context.tx.lock().expect("push callback mutex poisoned");
        *guard = None;
        context.notify.notify_one();
        // Reclaim the leaked Arc reference (see ChangeReceiverProxy::new)
        unsafe { Arc::from_raw(ctx as *const PushCallbackContext) };
        return false;
    }

    let ffi_event = unsafe { &*event };
    let wrapper = unsafe { *Box::from_raw(ffi_event.opaque as *mut SourceEventWrapper) };
    unsafe { drop(Box::from_raw(event)) };

    let guard = context.tx.lock().expect("push callback mutex poisoned");
    if let Some(tx) = guard.as_ref() {
        let ok = tx.send(Arc::new(wrapper)).is_ok();
        drop(guard);
        if ok {
            context.notify.notify_one();
            true
        } else {
            // Receiver was dropped — reclaim the leaked Arc
            unsafe { Arc::from_raw(ctx as *const PushCallbackContext) };
            false
        }
    } else {
        // Sender was already dropped — reclaim the leaked Arc
        drop(guard);
        unsafe { Arc::from_raw(ctx as *const PushCallbackContext) };
        false
    }
}

/// FFI state that owns the plugin-side receiver and manages its lifetime.
struct FfiReceiverState {
    drop_fn: extern "C" fn(*mut std::ffi::c_void),
    state: *mut std::ffi::c_void,
}

unsafe impl Send for FfiReceiverState {}
unsafe impl Sync for FfiReceiverState {}

impl Drop for FfiReceiverState {
    fn drop(&mut self) {
        (self.drop_fn)(self.state);
    }
}

/// Wraps an `FfiChangeReceiver` into a `ChangeReceiver<SourceEventWrapper>`.
///
/// On creation, calls `start_push_fn` to have the plugin push events via
/// callback into a local `std::sync::mpsc` channel. The `recv()` method
/// waits on a `tokio::sync::Notify` and then drains from the std channel,
/// avoiding any cross-cdylib tokio usage.
pub struct ChangeReceiverProxy {
    rx: std::sync::mpsc::Receiver<Arc<SourceEventWrapper>>,
    notify: Arc<tokio::sync::Notify>,
    /// Prevent the callback context from being freed while the plugin forwarder runs.
    _callback_ctx: Arc<PushCallbackContext>,
    /// Prevent the plugin-side FFI state from being freed while the forwarder runs.
    _ffi_state: Arc<FfiReceiverState>,
}

// Safety: All fields are Send+Sync safe
unsafe impl Send for ChangeReceiverProxy {}
unsafe impl Sync for ChangeReceiverProxy {}

impl ChangeReceiverProxy {
    pub fn new(inner: FfiChangeReceiver) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let notify = Arc::new(tokio::sync::Notify::new());

        let callback_ctx = Arc::new(PushCallbackContext {
            tx: std::sync::Mutex::new(Some(tx)),
            notify: notify.clone(),
        });

        let ffi_state = Arc::new(FfiReceiverState {
            drop_fn: inner.drop_fn,
            state: inner.state,
        });

        // Increment the Arc refcount for the plugin forwarder's use.
        // This leaked reference is reclaimed in the callback when
        // the channel closes (null event) or the callback returns false.
        let ctx_for_plugin = callback_ctx.clone();
        let ctx_ptr = Arc::into_raw(ctx_for_plugin) as *mut std::ffi::c_void;

        // Tell the plugin to start pushing events via the callback.
        // The plugin spawns a forwarder task on its own runtime.
        (inner.start_push_fn)(inner.state, change_push_callback, ctx_ptr);

        Self {
            rx,
            notify,
            _callback_ctx: callback_ctx,
            _ffi_state: ffi_state,
        }
    }
}

#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for ChangeReceiverProxy {
    async fn recv(&mut self) -> anyhow::Result<Arc<SourceEventWrapper>> {
        loop {
            // Try to receive without blocking first
            match self.rx.try_recv() {
                Ok(event) => return Ok(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // Wait for the plugin to notify us of a new event
                    self.notify.notified().await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(anyhow::anyhow!("Channel closed"));
                }
            }
        }
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
