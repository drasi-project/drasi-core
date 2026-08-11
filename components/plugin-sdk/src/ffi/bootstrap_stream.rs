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

//! Consumer side of the push-based bootstrap provider stream.
//!
//! A `BootstrapProviderVtable::bootstrap_fn` call returns an
//! [`FfiBootstrapStream`](super::vtables::FfiBootstrapStream): a push-based
//! event receiver plus a push-based result receiver. This module consumes
//! both, and is shared by the host proxy (`drasi-host-sdk`) and the
//! plugin-side proxy that lets a source plugin consume a provider living in
//! another cdylib.
//!
//! Backpressure: events are pushed by the producer's forwarder thread into a
//! **bounded** std `sync_channel`. When the async consumer falls behind, the
//! bridge fills, the push callback's blocking `send` stalls the forwarder,
//! the producer's bounded event channel fills, and the provider's own
//! `event_tx.send(..).await` pends. No link in the chain is unbounded.
//!
//! The cross-boundary channel uses `std::sync::mpsc` (not `tokio::sync::mpsc`)
//! because the callback runs on the producer's thread, and across cdylibs each
//! side has its own copy of tokio with incompatible internal state.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use drasi_lib::bootstrap::BootstrapResult;
use drasi_lib::channels::events::{BootstrapEvent, BootstrapEventSender};

use super::payload::consume_bootstrap_event;
use super::vtables::{
    FfiBootstrapEvent, FfiBootstrapReceiver, FfiBootstrapResult, FfiBootstrapResultReceiver,
};

/// Capacity of the bounded bridge between the producer's push callback and the
/// async consumer. Mirrors the `sync_channel(256)` used by the CDC
/// `ChangeReceiverProxy` and the source-plugin `BootstrapReceiverProxy`.
///
/// Together with [`BOOTSTRAP_PROVIDER_CAPACITY`] and the query-side channel,
/// this forms the aggregate in-flight cap per bootstrap stream:
/// `BOOTSTRAP_PROVIDER_CAPACITY + BOOTSTRAP_BRIDGE_CAPACITY + query channel
/// capacity` (plus a few events held by the forwarder threads themselves).
pub const BOOTSTRAP_BRIDGE_CAPACITY: usize = 256;

/// Capacity of the provider-side event channel created by
/// `build_bootstrap_provider_vtable`. Small by design: it only needs to keep
/// the forwarder busy, while the bound is what stalls the provider instead of
/// buffering the snapshot when the consumer falls behind (issue #686). See
/// [`BOOTSTRAP_BRIDGE_CAPACITY`] for how the aggregate in-flight cap
/// composes.
pub const BOOTSTRAP_PROVIDER_CAPACITY: usize = 100;

/// Context handed to the push callback. Holds the bounded bridge sender and a
/// tokio `Notify` to wake the async consumer.
struct PushCallbackContext {
    tx: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<BootstrapEvent>>>,
    notify: Arc<tokio::sync::Notify>,
    /// Set once the sentinel has reclaimed the producer's leaked Arc
    /// reference, so a duplicate sentinel from a buggy producer is a no-op
    /// instead of a double free. The consumer holds its own Arc reference,
    /// which keeps this flag readable after the reclaim.
    reclaimed: AtomicBool,
}

/// Push callback invoked by the producer's forwarder for each event.
/// Null event = forwarder-exit sentinel, guaranteed exactly once.
extern "C" fn bootstrap_push_callback(
    ctx: *mut std::ffi::c_void,
    event: *mut FfiBootstrapEvent,
) -> bool {
    // Catch panics to prevent unwinding across the extern "C" boundary (UB).
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        bootstrap_push_callback_inner(ctx, event)
    }))
    .unwrap_or(false)
}

fn bootstrap_push_callback_inner(
    ctx: *mut std::ffi::c_void,
    event: *mut FfiBootstrapEvent,
) -> bool {
    if ctx.is_null() {
        return false;
    }
    let context = unsafe { &*(ctx as *const PushCallbackContext) };

    if event.is_null() {
        // Forwarder-exit sentinel (the producer guarantees exactly one on
        // forwarder exit). This is the SOLE point at which the leaked Arc
        // reference is reclaimed; the `reclaimed` flag makes the reclaim
        // once-only so a duplicate sentinel from a buggy producer is a no-op
        // instead of a double free. Tolerate a poisoned lock so the reclaim
        // below always runs.
        if let Ok(mut guard) = context.tx.lock() {
            *guard = None;
        }
        context.notify.notify_one();
        if context
            .reclaimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            unsafe { Arc::from_raw(ctx as *const PushCallbackContext) };
        }
        return false;
    }

    let ffi_event = unsafe { &*event };
    // Decode into a consumer-owned BootstrapEvent and free the producer's
    // buffer via its own deallocator (issue #602: never reinterpret or drop
    // the other side's repr(Rust) memory).
    let decoded = unsafe { consume_bootstrap_event(ffi_event) };
    // Free the producer-allocated #[repr(C)] envelope (POD; no recursive Drop).
    unsafe { drop(Box::from_raw(event)) };

    let Some(bootstrap_event) = decoded else {
        // Undecodable payload: skip this event, keep the stream alive.
        return true;
    };

    // Forward to the consumer. On failure (receiver gone or poisoned lock)
    // return false to stop the forwarder, but do NOT reclaim the leaked Arc
    // here; the guaranteed sentinel does that exactly once. The send blocks
    // when the bridge is full, which is the backpressure path.
    let Ok(guard) = context.tx.lock() else {
        return false;
    };
    let Some(tx) = guard.as_ref() else {
        return false;
    };
    let ok = tx.send(bootstrap_event).is_ok();
    drop(guard);
    if ok {
        context.notify.notify_one();
        true
    } else {
        false
    }
}

/// Owns the producer-side receiver state and frees it on drop.
struct FfiReceiverState {
    drop_fn: extern "C" fn(*mut std::ffi::c_void),
    state: *mut std::ffi::c_void,
}

unsafe impl Send for FfiReceiverState {}
unsafe impl Sync for FfiReceiverState {}

impl Drop for FfiReceiverState {
    fn drop(&mut self) {
        // By the time the consumer is dropped the producer state is inert (the
        // event receiver was moved out by start_push_fn), so this only frees a
        // box. Guard against a buggy producer panicking in its drop_fn: an
        // unwind must not escape into our Drop.
        guarded_producer_drop(self.drop_fn, self.state);
    }
}

/// Invoke a producer-supplied `extern "C"` drop function behind a panic
/// barrier so a buggy producer cannot unwind into consumer code.
fn guarded_producer_drop(
    drop_fn: extern "C" fn(*mut std::ffi::c_void),
    state: *mut std::ffi::c_void,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop_fn(state)));
}

/// Free a bootstrap event receiver without consuming its stream.
///
/// Error-path cleanup for a partially-initialized `FfiBootstrapStream`:
/// releases the producer-side state via its `drop_fn` (which closes the
/// provider's event channel, unblocking the provider) so nothing leaks.
pub fn release_bootstrap_receiver(inner: FfiBootstrapReceiver) {
    guarded_producer_drop(inner.drop_fn, inner.state);
}

/// Free a bootstrap result receiver without consuming its result.
/// Error-path counterpart of [`release_bootstrap_receiver`].
pub fn release_result_receiver(inner: FfiBootstrapResultReceiver) {
    guarded_producer_drop(inner.drop_fn, inner.state);
}

/// Consumes an [`FfiBootstrapReceiver`]: starts the producer's push forwarder
/// into a bounded bridge and drains it asynchronously.
pub struct BootstrapStreamConsumer {
    rx: std::sync::mpsc::Receiver<BootstrapEvent>,
    notify: Arc<tokio::sync::Notify>,
    /// Keeps the callback context alive while the producer forwarder runs.
    _callback_ctx: Arc<PushCallbackContext>,
    /// Keeps the producer-side receiver state alive until consumption ends.
    _ffi_state: FfiReceiverState,
}

unsafe impl Send for BootstrapStreamConsumer {}

impl BootstrapStreamConsumer {
    /// Take ownership of the receiver and start push-based delivery.
    pub fn new(inner: FfiBootstrapReceiver) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(BOOTSTRAP_BRIDGE_CAPACITY);
        let notify = Arc::new(tokio::sync::Notify::new());

        let callback_ctx = Arc::new(PushCallbackContext {
            tx: std::sync::Mutex::new(Some(tx)),
            notify: notify.clone(),
            reclaimed: AtomicBool::new(false),
        });

        // Leak one Arc reference for the producer forwarder; reclaimed exactly
        // once by the sentinel callback.
        let ctx_ptr = Arc::into_raw(callback_ctx.clone()) as *mut std::ffi::c_void;
        (inner.start_push_fn)(inner.state, bootstrap_push_callback, ctx_ptr);

        Self {
            rx,
            notify,
            _callback_ctx: callback_ctx,
            _ffi_state: FfiReceiverState {
                drop_fn: inner.drop_fn,
                state: inner.state,
            },
        }
    }

    /// Forward every event into `tx` until the stream ends. Returns the number
    /// of events forwarded. Dropping the returned future cancels cleanly: the
    /// bridge receiver drops, the producer's next push fails, and the
    /// provider's send errors out.
    pub async fn forward_into(self, tx: &BootstrapEventSender) -> usize {
        let mut count = 0usize;
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    if tx.send(event).await.is_err() {
                        return count;
                    }
                    count += 1;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.notify.notified().await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return count;
                }
            }
        }
    }
}

/// Context for the result callback. Holds the oneshot sender.
struct ResultCallbackContext {
    tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<anyhow::Result<BootstrapResult>>>>,
    /// Set once the callback has reclaimed the producer's leaked Arc
    /// reference, so a duplicate callback from a buggy producer is a no-op
    /// instead of a double free. The consumer-held [`BootstrapResultGuard`]
    /// keeps this flag readable after the reclaim.
    reclaimed: AtomicBool,
}

/// Keeps the result callback context alive on the consumer side until the
/// result has been consumed. Hold this until the receiver returned by
/// [`wrap_result_receiver`] resolves; dropping it earlier is safe but narrows
/// the window in which a duplicate producer callback is a harmless no-op.
pub struct BootstrapResultGuard {
    _ctx: Arc<ResultCallbackContext>,
}

/// Result callback, invoked exactly once by the producer. Null result means
/// the provider ended without producing one; negative `event_count` is a
/// provider failure.
extern "C" fn bootstrap_result_callback(
    ctx: *mut std::ffi::c_void,
    result: *mut FfiBootstrapResult,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if ctx.is_null() {
            return;
        }
        let context = unsafe { &*(ctx as *const ResultCallbackContext) };
        let tx = match context.tx.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };

        // Decode (and free) the result envelope even when the sender is
        // already gone, so a duplicate delivery cannot leak the allocation.
        let outcome: anyhow::Result<BootstrapResult> = if result.is_null() {
            Err(anyhow::anyhow!("Bootstrap provider ended without a result"))
        } else {
            let ffi_result = unsafe { *Box::from_raw(result) };
            // Extract (and free) the provider error text unconditionally so
            // the buffer never leaks, whichever branch we take below.
            let error_text = if !ffi_result.error_ptr.is_null() && ffi_result.error_len > 0 {
                let bytes = unsafe {
                    std::slice::from_raw_parts(ffi_result.error_ptr, ffi_result.error_len)
                };
                let text = String::from_utf8_lossy(bytes).into_owned();
                if let Some(drop_fn) = ffi_result.error_drop_fn {
                    (drop_fn)(ffi_result.error_ptr as *mut u8, ffi_result.error_len);
                }
                Some(text)
            } else {
                None
            };
            if ffi_result.event_count < 0 {
                Err(match error_text {
                    Some(msg) => anyhow::anyhow!("Bootstrap failed: {msg}"),
                    None => {
                        anyhow::anyhow!("Bootstrap failed with code {}", ffi_result.event_count)
                    }
                })
            } else {
                let source_position = if !ffi_result.source_position_ptr.is_null()
                    && ffi_result.source_position_len > 0
                {
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            ffi_result.source_position_ptr,
                            ffi_result.source_position_len,
                        )
                    };
                    let owned = bytes::Bytes::copy_from_slice(bytes);
                    if let Some(drop_fn) = ffi_result.source_position_drop_fn {
                        (drop_fn)(
                            ffi_result.source_position_ptr as *mut u8,
                            ffi_result.source_position_len,
                        );
                    }
                    Some(owned)
                } else {
                    None
                };
                Ok(BootstrapResult {
                    event_count: ffi_result.event_count as usize,
                    source_position,
                })
            }
        };

        if let Some(tx) = tx {
            let _ = tx.send(outcome);
        }

        // Reclaim the producer's leaked Arc reference exactly once, and do it
        // last: after this the context may only be kept alive by the
        // consumer-held guard.
        if context
            .reclaimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            unsafe { Arc::from_raw(ctx as *const ResultCallbackContext) };
        }
    }));
}

/// Consume an [`FfiBootstrapResultReceiver`] into a oneshot. The producer
/// guarantees exactly one callback (a null result if it cannot produce one),
/// so the returned receiver always resolves; an `Err` from `await` means the
/// producer state was dropped before delivering anything. Keep the returned
/// [`BootstrapResultGuard`] alive until the receiver resolves.
pub fn wrap_result_receiver(
    inner: FfiBootstrapResultReceiver,
) -> (
    tokio::sync::oneshot::Receiver<anyhow::Result<BootstrapResult>>,
    BootstrapResultGuard,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ctx = Arc::new(ResultCallbackContext {
        tx: std::sync::Mutex::new(Some(tx)),
        reclaimed: AtomicBool::new(false),
    });
    // Leak one Arc reference for the producer; reclaimed exactly once by the
    // callback. The guard holds a second reference so the context stays alive
    // on the consumer side.
    let ctx_ptr = Arc::into_raw(ctx.clone()) as *mut std::ffi::c_void;
    (inner.start_fn)(inner.state, bootstrap_result_callback, ctx_ptr);
    // The producer's start_fn moves the pending result out of its state, so
    // the state can be released immediately (matches the subscribe path).
    guarded_producer_drop(inner.drop_fn, inner.state);
    (rx, BootstrapResultGuard { _ctx: ctx })
}
