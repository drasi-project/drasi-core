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
extern "C" fn change_push_callback(ctx: *mut std::ffi::c_void, event: *mut FfiSourceEvent) -> bool {
    // Catch panics to prevent unwinding across the extern "C" boundary (which is UB).
    // On panic, return false to signal shutdown. The leaked Arc is NOT reclaimed here
    // because we cannot know which code path panicked. It will be reclaimed when the
    // forwarder task exits (via the null-event callback or send failure).
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        change_push_callback_inner(ctx, event)
    }));
    match res {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[DIAG-SRC] change_push_callback PANIC ctx={:p} event={:p}", ctx, event);
            false
        }
    }
}

fn change_push_callback_inner(ctx: *mut std::ffi::c_void, event: *mut FfiSourceEvent) -> bool {
    let context = unsafe { &*(ctx as *const PushCallbackContext) };

    if event.is_null() {
        eprintln!("[DIAG-SRC] change_push_callback NULL ctx={:p}", ctx);
        // Channel closed — drop the sender to signal the host receiver
        {
            let mut guard = context.tx.lock().expect("push callback mutex poisoned");
            *guard = None;
            context.notify.notify_one();
            // guard must be dropped BEFORE reclaiming the Arc, otherwise
            // MutexGuard::drop runs after the PushCallbackContext is freed.
        }
        // Reclaim the leaked Arc reference (see ChangeReceiverProxy::new)
        unsafe { Arc::from_raw(ctx as *const PushCallbackContext) };
        eprintln!("[DIAG-SRC] change_push_callback NULL DONE ctx={:p}", ctx);
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
            eprintln!("[DIAG-SRC] change_push_callback SEND_ERR ctx={:p}", ctx);
            unsafe { Arc::from_raw(ctx as *const PushCallbackContext) };
            false
        }
    } else {
        // Sender was already dropped — reclaim the leaked Arc
        drop(guard);
        eprintln!("[DIAG-SRC] change_push_callback NO_TX ctx={:p}", ctx);
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
        eprintln!("[DIAG-SRC] FfiReceiverState::drop ENTER state={:p}", self.state);
        // Run plugin drop on a dedicated thread to avoid initializing
        // plugin TLS on the caller's thread.
        let drop_fn = self.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.state);
        // Safety: state is a valid pointer owned by the plugin
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();
        eprintln!("[DIAG-SRC] FfiReceiverState::drop EXIT");
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

impl Drop for ChangeReceiverProxy {
    fn drop(&mut self) {
        let ctx_ptr = Arc::as_ptr(&self._callback_ctx) as *const std::ffi::c_void;
        eprintln!("[DIAG-SRC] ChangeReceiverProxy::drop ENTER ctx={:p}", ctx_ptr);
    }
}

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

/// Context passed to the bootstrap push callback. Holds the std mpsc sender
/// and a tokio Notify to wake the host receiver.
struct BootstrapPushCallbackContext {
    tx: std::sync::Mutex<
        Option<std::sync::mpsc::SyncSender<drasi_lib::channels::events::BootstrapEvent>>,
    >,
    notify: Arc<tokio::sync::Notify>,
}

/// The push callback invoked by the plugin forwarder for each bootstrap event.
extern "C" fn bootstrap_push_callback(
    ctx: *mut std::ffi::c_void,
    event: *mut drasi_plugin_sdk::ffi::FfiBootstrapEvent,
) -> bool {
    // Catch panics to prevent unwinding across the extern "C" boundary.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        bootstrap_push_callback_inner(ctx, event)
    }))
    .unwrap_or(false)
}

fn bootstrap_push_callback_inner(
    ctx: *mut std::ffi::c_void,
    event: *mut drasi_plugin_sdk::ffi::FfiBootstrapEvent,
) -> bool {
    let context = unsafe { &*(ctx as *const BootstrapPushCallbackContext) };

    if event.is_null() {
        // Stream exhausted — drop the sender to signal completion
        {
            let mut guard = context
                .tx
                .lock()
                .expect("bootstrap push callback mutex poisoned");
            *guard = None;
            context.notify.notify_one();
            // guard must be dropped BEFORE reclaiming the Arc
        }
        // Reclaim the leaked Arc reference
        unsafe { Arc::from_raw(ctx as *const BootstrapPushCallbackContext) };
        return false;
    }

    let ffi_event = unsafe { &*event };
    let bootstrap_event = unsafe {
        *Box::from_raw(ffi_event.opaque as *mut drasi_lib::channels::events::BootstrapEvent)
    };
    unsafe { drop(Box::from_raw(event)) };

    let guard = context
        .tx
        .lock()
        .expect("bootstrap push callback mutex poisoned");
    if let Some(tx) = guard.as_ref() {
        let ok = tx.send(bootstrap_event).is_ok();
        drop(guard);
        if ok {
            context.notify.notify_one();
            true
        } else {
            unsafe { Arc::from_raw(ctx as *const BootstrapPushCallbackContext) };
            false
        }
    } else {
        drop(guard);
        unsafe { Arc::from_raw(ctx as *const BootstrapPushCallbackContext) };
        false
    }
}

/// FFI state that owns the plugin-side bootstrap receiver and manages its lifetime.
struct FfiBootstrapReceiverState {
    drop_fn: extern "C" fn(*mut std::ffi::c_void),
    state: *mut std::ffi::c_void,
}

unsafe impl Send for FfiBootstrapReceiverState {}
unsafe impl Sync for FfiBootstrapReceiverState {}

impl Drop for FfiBootstrapReceiverState {
    fn drop(&mut self) {
        // Run plugin drop on a dedicated thread to avoid initializing
        // plugin TLS on the caller's thread (matches FfiReceiverState::drop).
        let drop_fn = self.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.state);
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();
    }
}

/// Wraps an `FfiBootstrapReceiver` as a tokio mpsc Receiver of `BootstrapEvent`.
///
/// Uses a push-based model matching `ChangeReceiverProxy`: the plugin spawns
/// a forwarder that pushes events via callback into a `std::sync::mpsc` channel.
/// `into_mpsc_receiver()` converts this into a `tokio::sync::mpsc::Receiver`
/// by draining via `try_recv` + `Notify::notified().await`.
pub struct BootstrapReceiverProxy {
    rx: std::sync::mpsc::Receiver<drasi_lib::channels::events::BootstrapEvent>,
    notify: Arc<tokio::sync::Notify>,
    _callback_ctx: Arc<BootstrapPushCallbackContext>,
    _ffi_state: Arc<FfiBootstrapReceiverState>,
}

unsafe impl Send for BootstrapReceiverProxy {}
unsafe impl Sync for BootstrapReceiverProxy {}

impl BootstrapReceiverProxy {
    pub fn new(inner: drasi_plugin_sdk::ffi::FfiBootstrapReceiver) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let notify = Arc::new(tokio::sync::Notify::new());

        let callback_ctx = Arc::new(BootstrapPushCallbackContext {
            tx: std::sync::Mutex::new(Some(tx)),
            notify: notify.clone(),
        });

        let ffi_state = Arc::new(FfiBootstrapReceiverState {
            drop_fn: inner.drop_fn,
            state: inner.state,
        });

        // Leak an Arc reference for the plugin forwarder; reclaimed in callback
        let ctx_for_plugin = callback_ctx.clone();
        let ctx_ptr = Arc::into_raw(ctx_for_plugin) as *mut std::ffi::c_void;

        // Start push-based forwarding on the plugin runtime
        (inner.start_push_fn)(inner.state, bootstrap_push_callback, ctx_ptr);

        Self {
            rx,
            notify,
            _callback_ctx: callback_ctx,
            _ffi_state: ffi_state,
        }
    }

    /// Convert into a tokio mpsc Receiver by spawning a forwarding task.
    pub fn into_mpsc_receiver(self) -> drasi_lib::channels::events::BootstrapEventReceiver {
        let (tx, out_rx) = tokio::sync::mpsc::channel(256);
        let rx = self.rx;
        let notify = self.notify.clone();
        // Keep FFI state and callback context alive until forwarding completes
        let _ffi_state = self._ffi_state;
        let _callback_ctx = self._callback_ctx;

        tokio::spawn(async move {
            let _ffi = _ffi_state;
            let _ctx = _callback_ctx;
            loop {
                // Drain all available events
                loop {
                    match rx.try_recv() {
                        Ok(event) => {
                            if tx.send(event).await.is_err() {
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                notify.notified().await;
            }
        });

        out_rx
    }
}
