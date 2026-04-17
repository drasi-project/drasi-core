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

/// Sentinel pointer value sent by the plugin forwarder after its loop exits.
/// The callback recognises this non-null, obviously-invalid address and uses
/// it to signal `forwarder_done`. Must match the value in `vtable_gen.rs`.
const CHANGE_FORWARDER_SENTINEL: usize = 1;

/// Tracks whether the plugin forwarder task has fully exited.
struct ForwarderDone {
    done: std::sync::Mutex<bool>,
    cvar: std::sync::Condvar,
}

impl ForwarderDone {
    fn new() -> Self {
        Self {
            done: std::sync::Mutex::new(false),
            cvar: std::sync::Condvar::new(),
        }
    }

    fn signal(&self) {
        let mut done = self.done.lock().expect("forwarder_done mutex poisoned");
        *done = true;
        self.cvar.notify_all();
    }

    fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        let done = self.done.lock().expect("forwarder_done mutex poisoned");
        if *done {
            return true;
        }
        let (guard, _) = self
            .cvar
            .wait_timeout(done, timeout)
            .expect("forwarder_done mutex poisoned");
        *guard
    }
}

/// Context passed to the push callback. Holds the std mpsc sender and a
/// tokio Notify to wake the host receiver.
struct PushCallbackContext {
    tx: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<Arc<SourceEventWrapper>>>>,
    notify: Arc<tokio::sync::Notify>,
    forwarder_done: Arc<ForwarderDone>,
}

/// The push callback invoked by the plugin forwarder for each event.
/// Uses `std::sync::mpsc` which is safe to call from any runtime.
extern "C" fn change_push_callback(ctx: *mut std::ffi::c_void, event: *mut FfiSourceEvent) -> bool {
    // Catch panics to prevent unwinding across the extern "C" boundary (which is UB).
    // On panic, return false to signal shutdown. The forwarder will send a
    // sentinel after its loop breaks, which signals forwarder_done.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        change_push_callback_inner(ctx, event)
    }))
    .unwrap_or(false)
}

fn change_push_callback_inner(ctx: *mut std::ffi::c_void, event: *mut FfiSourceEvent) -> bool {
    // Sentinel check: the plugin forwarder sends this after its loop exits.
    // Clone the ForwarderDone Arc before signaling so the Condvar remains
    // alive even if the host drops PushCallbackContext in response.
    if !event.is_null() && (event as usize) == CHANGE_FORWARDER_SENTINEL {
        let context = unsafe { &*(ctx as *const PushCallbackContext) };
        let forwarder_done = context.forwarder_done.clone();
        forwarder_done.signal();
        return false;
    }

    let context = unsafe { &*(ctx as *const PushCallbackContext) };

    if event.is_null() {
        // Channel closed — drop the sender to signal the host receiver.
        // The forwarder will send a sentinel after this, which signals
        // forwarder_done. We do NOT free ctx here.
        let mut guard = context.tx.lock().expect("push callback mutex poisoned");
        *guard = None;
        context.notify.notify_one();
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
            // Receiver was dropped — forwarder will send sentinel next
            false
        }
    } else {
        drop(guard);
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
        // Run plugin drop on a dedicated thread to avoid initializing
        // plugin TLS on the caller's thread.
        let drop_fn = self.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.state);
        // Safety: state is a valid pointer owned by the plugin
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();
    }
}

/// Wraps an `FfiChangeReceiver` into a `ChangeReceiver<SourceEventWrapper>`.
///
/// On creation, calls `start_push_fn` to have the plugin push events via
/// callback into a local `std::sync::mpsc` channel. The `recv()` method
/// waits on a `tokio::sync::Notify` and then drains from the std channel,
/// avoiding any cross-cdylib tokio usage.
///
/// On drop, the proxy closes the host channel, signals shutdown to the
/// plugin forwarder, then waits for the forwarder to send a sentinel
/// confirming it has fully exited before releasing any resources.
pub struct ChangeReceiverProxy {
    rx: Option<std::sync::mpsc::Receiver<Arc<SourceEventWrapper>>>,
    notify: Arc<tokio::sync::Notify>,
    /// Keeps the callback context alive for the duration of the plugin forwarder.
    /// The raw pointer passed to the plugin is derived via `Arc::as_ptr`.
    _callback_ctx: Arc<PushCallbackContext>,
    /// Triggers plugin-side cleanup (shutdown + handle free) when dropped.
    _ffi_state: Option<Arc<FfiReceiverState>>,
    /// Shared flag signalled by the forwarder's sentinel callback.
    forwarder_done: Arc<ForwarderDone>,
}

// Safety: All fields are Send+Sync safe
unsafe impl Send for ChangeReceiverProxy {}
unsafe impl Sync for ChangeReceiverProxy {}

impl ChangeReceiverProxy {
    pub fn new(inner: FfiChangeReceiver) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let notify = Arc::new(tokio::sync::Notify::new());
        let forwarder_done = Arc::new(ForwarderDone::new());

        let callback_ctx = Arc::new(PushCallbackContext {
            tx: std::sync::Mutex::new(Some(tx)),
            notify: notify.clone(),
            forwarder_done: forwarder_done.clone(),
        });

        let ffi_state = Arc::new(FfiReceiverState {
            drop_fn: inner.drop_fn,
            state: inner.state,
        });

        // Pass a raw pointer derived from the Arc — the host retains ownership.
        // The pointer is valid as long as `_callback_ctx` is alive.
        let ctx_ptr = Arc::as_ptr(&callback_ctx) as *mut std::ffi::c_void;

        // Tell the plugin to start pushing events via the callback.
        // The plugin spawns a forwarder task on its own runtime.
        (inner.start_push_fn)(inner.state, change_push_callback, ctx_ptr);

        Self {
            rx: Some(rx),
            notify,
            _callback_ctx: callback_ctx,
            _ffi_state: Some(ffi_state),
            forwarder_done,
        }
    }
}

impl Drop for ChangeReceiverProxy {
    fn drop(&mut self) {
        // 1. Close the host channel so the forwarder's callback send fails.
        self.rx.take();

        // 2. Signal shutdown on the plugin side (triggers select! shutdown branch).
        //    FfiReceiverState::drop → change_receiver_drop → notify_one.
        self._ffi_state.take();

        // 3. Wait for the forwarder to send its sentinel, confirming it has
        //    exited the loop and will not touch the callback context again.
        if !self
            .forwarder_done
            .wait_timeout(std::time::Duration::from_secs(5))
        {
            // Timeout — the forwarder is still alive. Leak the callback
            // context to prevent use-after-free if it accesses ctx later.
            std::mem::forget(self._callback_ctx.clone());
        }
        // After this, normal field drops run:
        //   _callback_ctx → PushCallbackContext freed (safe, forwarder is done)
        //   forwarder_done → ForwarderDone freed (safe, no more waiters)
    }
}

#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for ChangeReceiverProxy {
    async fn recv(&mut self) -> anyhow::Result<Arc<SourceEventWrapper>> {
        loop {
            // Scope the reference to `rx` so it doesn't live across the await.
            // `std::sync::mpsc::Receiver` is not Sync, so `&Receiver` is not Send.
            let try_result = {
                let rx = self
                    .rx
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Receiver closed"))?;
                rx.try_recv()
            };
            match try_result {
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

/// Sentinel pointer value for the bootstrap forwarder (same concept as change forwarder).
const BOOTSTRAP_FORWARDER_SENTINEL: usize = 1;

/// Context passed to the bootstrap push callback. Holds the std mpsc sender
/// and a tokio Notify to wake the host receiver.
struct BootstrapPushCallbackContext {
    tx: std::sync::Mutex<
        Option<std::sync::mpsc::SyncSender<drasi_lib::channels::events::BootstrapEvent>>,
    >,
    notify: Arc<tokio::sync::Notify>,
    forwarder_done: Arc<ForwarderDone>,
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
    // Sentinel check
    if !event.is_null() && (event as usize) == BOOTSTRAP_FORWARDER_SENTINEL {
        let context = unsafe { &*(ctx as *const BootstrapPushCallbackContext) };
        let forwarder_done = context.forwarder_done.clone();
        forwarder_done.signal();
        return false;
    }

    let context = unsafe { &*(ctx as *const BootstrapPushCallbackContext) };

    if event.is_null() {
        // Stream exhausted — drop the sender to signal completion.
        // Forwarder will send sentinel next.
        let mut guard = context
            .tx
            .lock()
            .expect("bootstrap push callback mutex poisoned");
        *guard = None;
        context.notify.notify_one();
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
            false
        }
    } else {
        drop(guard);
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
    rx: Option<std::sync::mpsc::Receiver<drasi_lib::channels::events::BootstrapEvent>>,
    notify: Arc<tokio::sync::Notify>,
    _callback_ctx: Arc<BootstrapPushCallbackContext>,
    _ffi_state: Option<Arc<FfiBootstrapReceiverState>>,
    forwarder_done: Arc<ForwarderDone>,
}

unsafe impl Send for BootstrapReceiverProxy {}
unsafe impl Sync for BootstrapReceiverProxy {}

impl BootstrapReceiverProxy {
    pub fn new(inner: drasi_plugin_sdk::ffi::FfiBootstrapReceiver) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        let notify = Arc::new(tokio::sync::Notify::new());
        let forwarder_done = Arc::new(ForwarderDone::new());

        let callback_ctx = Arc::new(BootstrapPushCallbackContext {
            tx: std::sync::Mutex::new(Some(tx)),
            notify: notify.clone(),
            forwarder_done: forwarder_done.clone(),
        });

        let ffi_state = Arc::new(FfiBootstrapReceiverState {
            drop_fn: inner.drop_fn,
            state: inner.state,
        });

        // Host-owned pointer — no Arc leak
        let ctx_ptr = Arc::as_ptr(&callback_ctx) as *mut std::ffi::c_void;

        // Start push-based forwarding on the plugin runtime
        (inner.start_push_fn)(inner.state, bootstrap_push_callback, ctx_ptr);

        Self {
            rx: Some(rx),
            notify,
            _callback_ctx: callback_ctx,
            _ffi_state: Some(ffi_state),
            forwarder_done,
        }
    }

    /// Convert into a tokio mpsc Receiver by spawning a forwarding task.
    pub fn into_mpsc_receiver(mut self) -> drasi_lib::channels::events::BootstrapEventReceiver {
        let (tx, out_rx) = tokio::sync::mpsc::channel(256);
        let rx = self.rx.take().expect("rx should be Some");
        let notify = self.notify.clone();
        // Take ownership of FFI state and callback context for the spawned task
        let ffi_state = self._ffi_state.take();
        let callback_ctx = self._callback_ctx.clone();
        let forwarder_done = self.forwarder_done.clone();
        // self will be dropped after this, but Drop is a no-op when fields are taken

        tokio::spawn(async move {
            let _ffi = ffi_state;
            let _ctx = callback_ctx;
            let _fd = forwarder_done;
            loop {
                // Drain all available events
                loop {
                    match rx.try_recv() {
                        Ok(event) => {
                            if tx.send(event).await.is_err() {
                                // Wait for plugin forwarder to finish before dropping state
                                wait_forwarder_done_async(&_fd).await;
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            wait_forwarder_done_async(&_fd).await;
                            return;
                        }
                    }
                }
                notify.notified().await;
            }
        });

        out_rx
    }
}

/// Helper to wait for forwarder completion in an async context.
async fn wait_forwarder_done_async(fd: &Arc<ForwarderDone>) {
    let fd = fd.clone();
    let _ = tokio::task::spawn_blocking(move || {
        fd.wait_timeout(std::time::Duration::from_secs(5));
    })
    .await;
}

impl Drop for BootstrapReceiverProxy {
    fn drop(&mut self) {
        // If into_mpsc_receiver already consumed the fields, this is a no-op.
        if self.rx.is_none() && self._ffi_state.is_none() {
            return;
        }
        self.rx.take();
        self._ffi_state.take();
        if !self
            .forwarder_done
            .wait_timeout(std::time::Duration::from_secs(5))
        {
            std::mem::forget(self._callback_ctx.clone());
        }
    }
}
