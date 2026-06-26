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

use drasi_lib::channels::events::{BootstrapEvent, SourceEventWrapper};
use drasi_lib::channels::ChangeReceiver;
use drasi_plugin_sdk::ffi::payload::{consume_bootstrap_event, consume_source_event};
use drasi_plugin_sdk::ffi::{FfiBootstrapEvent, FfiChangeReceiver, FfiSourceEvent};

/// Decode a plugin-sent `FfiSourceEvent` into a **host-owned** `SourceEventWrapper`.
///
/// The payload crosses the cdylib boundary as MessagePack bytes (issue #602): the
/// host deserializes its own copy and frees the plugin's buffer via the
/// plugin-supplied `payload_drop_fn`. The host never reads or drops the plugin's
/// `repr(Rust)` memory. Returns `None` if the payload cannot be decoded (the
/// plugin buffer is freed regardless). Delegates to the canonical
/// [`consume_source_event`], where the size/null hardening lives.
fn decode_source_event(ffi_event: &FfiSourceEvent) -> Option<SourceEventWrapper> {
    unsafe { consume_source_event(ffi_event) }
}

/// Decode a plugin-sent `FfiBootstrapEvent` into a host-owned `BootstrapEvent`.
/// See [`decode_source_event`] for the ownership contract (issue #602).
fn decode_bootstrap_event(ffi_event: &FfiBootstrapEvent) -> Option<BootstrapEvent> {
    unsafe { consume_bootstrap_event(ffi_event) }
}

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
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        change_push_callback_inner(ctx, event)
    }))
    .unwrap_or(false)
}

fn change_push_callback_inner(ctx: *mut std::ffi::c_void, event: *mut FfiSourceEvent) -> bool {
    let context = unsafe { &*(ctx as *const PushCallbackContext) };

    if event.is_null() {
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
        return false;
    }

    let ffi_event = unsafe { &*event };
    let decoded = decode_source_event(ffi_event);
    // Free the plugin-allocated `#[repr(C)]` envelope (POD; no recursive Drop).
    unsafe { drop(Box::from_raw(event)) };

    let Some(wrapper) = decoded else {
        // Undecodable payload — skip without tearing down the stream.
        return true;
    };

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
        // Run plugin drop on the shared worker thread to avoid TLS destructor
        // races on macOS arm64 (see drop_worker module for details).
        let drop_fn = self.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.state);
        // Safety: state is a valid pointer owned by the plugin
        super::drop_worker::execute_drop_fn(drop_fn, state);
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
    let decoded = decode_bootstrap_event(ffi_event);
    // Free the plugin-allocated `#[repr(C)]` envelope (POD; no recursive Drop).
    unsafe { drop(Box::from_raw(event)) };

    let Some(bootstrap_event) = decoded else {
        // Undecodable payload — skip without tearing down the stream.
        return true;
    };

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
        // Run plugin drop on the shared worker thread to avoid TLS destructor
        // races on macOS arm64 (see drop_worker module for details, matches
        // FfiReceiverState::drop).
        let drop_fn = self.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);
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

#[cfg(test)]
mod ownership_tests {
    //! T2 (part 2) — host-side FFI ownership for issue #602.
    //!
    //! These tests drive the **real** host consumer (`decode_source_event` /
    //! `decode_bootstrap_event`) with an `FfiSourceEvent`/`FfiBootstrapEvent`
    //! constructed exactly as the plugin producer does, using a call-counting
    //! `payload_drop_fn`. They assert that:
    //!   1. the host rebuilds a value-equal, host-owned event, and
    //!   2. the host frees the plugin's payload buffer **exactly once** via the
    //!      plugin-supplied `drop_fn` — never reinterpreting/dropping the
    //!      plugin's `repr(Rust)` memory.

    use super::*;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use drasi_lib::channels::events::SourceEvent;
    use drasi_plugin_sdk::ffi::payload::{BootstrapEventPayload, SourceEventPayload};
    use drasi_plugin_sdk::ffi::FfiChangeOp;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SRC_DROPS_VALID: AtomicUsize = AtomicUsize::new(0);
    static SRC_DROPS_INVALID: AtomicUsize = AtomicUsize::new(0);
    static BOOT_DROPS: AtomicUsize = AtomicUsize::new(0);
    static BOOT_DROPS_INVALID: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn counting_drop_src_valid(ptr: *mut u8, len: usize) {
        SRC_DROPS_VALID.fetch_add(1, Ordering::SeqCst);
        free_bytes(ptr, len);
    }

    extern "C" fn counting_drop_src_invalid(ptr: *mut u8, len: usize) {
        SRC_DROPS_INVALID.fetch_add(1, Ordering::SeqCst);
        free_bytes(ptr, len);
    }

    extern "C" fn counting_drop_boot(ptr: *mut u8, len: usize) {
        BOOT_DROPS.fetch_add(1, Ordering::SeqCst);
        free_bytes(ptr, len);
    }

    extern "C" fn counting_drop_boot_invalid(ptr: *mut u8, len: usize) {
        BOOT_DROPS_INVALID.fetch_add(1, Ordering::SeqCst);
        free_bytes(ptr, len);
    }

    fn free_bytes(ptr: *mut u8, len: usize) {
        if !ptr.is_null() && len > 0 {
            unsafe {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
            }
        }
    }

    fn node() -> Element {
        let mut props = ElementPropertyMap::new();
        props.insert("plate", ElementValue::String(std::sync::Arc::from("A1234")));
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("src-1", "vehicles:A1234"),
                labels: std::sync::Arc::from(vec![std::sync::Arc::from("vehicles")]),
                effective_from: 1_771_000_000_000,
            },
            properties: props,
        }
    }

    /// Mirrors the plugin producer: serialize the payload (named MessagePack)
    /// into a heap buffer and build the `#[repr(C)]` envelope.
    fn make_source_event(
        payload: &SourceEventPayload,
        drop_fn: extern "C" fn(*mut u8, usize),
    ) -> *mut FfiSourceEvent {
        let bytes = rmp_serde::to_vec_named(payload).unwrap();
        let payload_len = bytes.len();
        let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
        Box::into_raw(Box::new(FfiSourceEvent {
            payload_ptr,
            payload_len,
            payload_drop_fn: Some(drop_fn),
            op: FfiChangeOp::Insert,
            timestamp_us: payload.timestamp_us,
        }))
    }

    #[test]
    fn decode_source_event_rebuilds_and_frees_buffer_once() {
        let payload = SourceEventPayload {
            source_id: "src-1".to_string(),
            event: SourceEvent::Change(SourceChange::Insert { element: node() }),
            timestamp_us: 1_771_000_000_000_000,
            sequence: Some(99),
            source_position: Some(b"binlog:1".to_vec()),
        };
        let raw = make_source_event(&payload, counting_drop_src_valid);
        let ffi = unsafe { &*raw };

        let wrapper = decode_source_event(ffi).expect("host-owned wrapper");
        assert_eq!(wrapper.source_id, "src-1");
        assert_eq!(wrapper.sequence, Some(99));
        assert_eq!(wrapper.source_position.as_deref(), Some(&b"binlog:1"[..]));
        assert_eq!(wrapper.event, payload.event);

        // The plugin's payload buffer was freed exactly once, by the host
        // calling the plugin-supplied drop_fn.
        assert_eq!(SRC_DROPS_VALID.load(Ordering::SeqCst), 1);

        // The host owns `wrapper`; dropping it must not touch the (already freed)
        // plugin buffer or double-free.
        drop(wrapper);
        assert_eq!(SRC_DROPS_VALID.load(Ordering::SeqCst), 1);

        // Free the #[repr(C)] envelope (as the real callback does).
        unsafe { drop(Box::from_raw(raw)) };
    }

    #[test]
    fn decode_source_event_handles_undecodable_payload_without_leak() {
        // Garbage bytes that are not a valid SourceEventPayload.
        let bytes = vec![0xFFu8; 8];
        let payload_len = bytes.len();
        let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
        let raw = Box::into_raw(Box::new(FfiSourceEvent {
            payload_ptr,
            payload_len,
            payload_drop_fn: Some(counting_drop_src_invalid),
            op: FfiChangeOp::Insert,
            timestamp_us: 0,
        }));
        let ffi = unsafe { &*raw };

        assert!(decode_source_event(ffi).is_none());
        // Buffer still freed exactly once even though decode failed.
        assert_eq!(SRC_DROPS_INVALID.load(Ordering::SeqCst), 1);
        unsafe { drop(Box::from_raw(raw)) };
    }

    #[test]
    fn decode_bootstrap_event_rebuilds_and_frees_buffer_once() {
        let payload = BootstrapEventPayload {
            source_id: "src-1".to_string(),
            change: SourceChange::Insert { element: node() },
            timestamp_us: 1_771_000_000_000_000,
            sequence: 5,
        };
        let bytes = rmp_serde::to_vec_named(&payload).unwrap();
        let payload_len = bytes.len();
        let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
        let raw = Box::into_raw(Box::new(FfiBootstrapEvent {
            payload_ptr,
            payload_len,
            payload_drop_fn: Some(counting_drop_boot),
            timestamp_us: payload.timestamp_us,
            sequence: 5,
        }));
        let ffi = unsafe { &*raw };

        let event = decode_bootstrap_event(ffi).expect("host-owned bootstrap event");
        assert_eq!(event.source_id, "src-1");
        assert_eq!(event.sequence, 5);
        assert_eq!(event.change, payload.change);
        assert_eq!(BOOT_DROPS.load(Ordering::SeqCst), 1);
        unsafe { drop(Box::from_raw(raw)) };
    }

    #[test]
    fn decode_bootstrap_event_handles_undecodable_payload_without_leak() {
        // Garbage bytes that are not a valid BootstrapEventPayload.
        let bytes = vec![0xFFu8; 8];
        let payload_len = bytes.len();
        let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
        let raw = Box::into_raw(Box::new(FfiBootstrapEvent {
            payload_ptr,
            payload_len,
            payload_drop_fn: Some(counting_drop_boot_invalid),
            timestamp_us: 0,
            sequence: 0,
        }));
        let ffi = unsafe { &*raw };

        assert!(decode_bootstrap_event(ffi).is_none());
        // Buffer still freed exactly once even though decode failed.
        assert_eq!(BOOT_DROPS_INVALID.load(Ordering::SeqCst), 1);
        unsafe { drop(Box::from_raw(raw)) };
    }
}
