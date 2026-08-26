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

//! Host-side proxy for Reaction and ReactionPluginDescriptor.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::identity::IdentityProvider;
use drasi_lib::reactions::Reaction;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::{ComponentStatus, ReactionRuntimeContext};
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;
use drasi_plugin_sdk::ffi::payload::encode_query_result;
use drasi_plugin_sdk::ffi::{
    FfiBootstrapContext, FfiCheckpoint, FfiCheckpointResult, FfiComponentStatus, FfiOutboxIterator,
    FfiOutboxIteratorResponse, FfiQueryResult, FfiResult, FfiResultPushControl, FfiRuntimeContext,
    FfiSnapshotIterator, FfiSnapshotIteratorResponse, FfiStr, ReactionPluginVtable, ReactionVtable,
    FFI_RESULT_PUSH_ACK_ERROR, FFI_RESULT_PUSH_ACK_OK, FFI_RESULT_PUSH_FORWARDER_EXIT,
    FFI_RESULT_PUSH_PROTOCOL_VERSION, FFI_RESULT_PUSH_REQUEST,
};
use libloading::Library;

use crate::snapshot_fetcher_bridge::SnapshotFetcherVtableBuilder;
use crate::state_store_bridge::StateStoreVtableBuilder;

/// Wraps a `ReactionVtable` into a DrasiLib `Reaction` trait implementation.
pub struct ReactionProxy {
    vtable: ReactionVtable,
    _library: Arc<Library>,
    cached_id: String,
    cached_type_name: String,
    /// Per-instance callback context for plugin-emitted log/lifecycle callbacks.
    ///
    /// Stored as an `Arc` whose strong count was bumped by `Arc::into_raw` when
    /// the raw pointer was handed to the plugin. The host's `Arc` is kept here
    /// so the proxy holds at least two strong references; on Drop the host's
    /// `Arc` is `mem::forget`-ed unconditionally so any **late** log/lifecycle
    /// callback emitted by the plugin (after `stop()` returns) still finds a
    /// valid pointer. The cdylib itself is intentionally leaked process-wide
    /// (see `host-sdk/src/loader.rs`), so the small per-instance `Arc` leak is
    /// acceptable in exchange for closing the late-callback UAF window.
    _callback_ctx: std::sync::Mutex<Option<Arc<crate::callbacks::InstanceCallbackContext>>>,
    /// Bounded bridge for serialized, acknowledged result delivery. Created on
    /// start and closed on stop/drop.
    result_tx: std::sync::Mutex<Option<std::sync::mpsc::SyncSender<ResultDelivery>>>,
    /// Keep the callback context alive for the lifetime of the forwarder.
    _push_ctx: std::sync::Mutex<Option<Arc<ResultPushContext>>>,
    /// Per-reaction identity provider set programmatically via
    /// [`Reaction::set_identity_provider`]. When present, it takes precedence
    /// over any instance-wide provider supplied via
    /// [`ReactionRuntimeContext::identity_provider`] during
    /// [`Reaction::initialize`].
    identity_provider: std::sync::Mutex<Option<Arc<dyn IdentityProvider>>>,
}

/// Context for the serialized result request/acknowledgement callback.
struct ResultPushContext {
    rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<ResultDelivery>>>,
    /// All host enqueue calls that have not been acknowledged. This includes
    /// items still in the bounded bridge, so a forwarder failure can resolve
    /// every waiting host future rather than stranding one behind the failure.
    pending: std::sync::Mutex<Vec<Arc<ResultAck>>>,
    /// The one item currently held by the serialized plugin forwarder.
    current: std::sync::Mutex<Option<Arc<ResultAck>>>,
    accepting: std::sync::atomic::AtomicBool,
    /// Signaled when the plugin-side forwarder task has fully exited its loop
    /// and will no longer access the `ReactionWrapper`. The forwarder signals
    /// this by calling the callback one final time with the sentinel parameter,
    /// AFTER breaking out of its processing loop.
    forwarder_done: std::sync::Mutex<bool>,
    forwarder_done_cv: std::sync::Condvar,
}

struct ResultDelivery {
    result: drasi_lib::channels::QueryResult,
    ack: Arc<ResultAck>,
}

struct ResultAck {
    completion: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<(), String>>>>,
}

impl ResultAck {
    fn complete(&self, result: Result<(), String>) {
        if let Ok(mut completion) = self.completion.lock() {
            if let Some(tx) = completion.take() {
                let _ = tx.send(result);
            }
        }
    }
}

impl ResultPushContext {
    fn register(&self, ack: Arc<ResultAck>) -> Result<(), String> {
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Reaction result forwarder is not accepting deliveries".into());
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Result acknowledgement state is poisoned".to_string())?;
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Reaction result forwarder is not accepting deliveries".into());
        }
        pending.push(ack);
        Ok(())
    }

    fn acknowledge_current(&self, result: Result<(), String>) {
        let current = self
            .current
            .lock()
            .ok()
            .and_then(|mut current| current.take());
        if let Some(ack) = current {
            ack.complete(result);
            if let Ok(mut pending) = self.pending.lock() {
                pending.retain(|candidate| !Arc::ptr_eq(candidate, &ack));
            }
        } else {
            log::warn!("Reaction result forwarder acknowledged without a pending delivery");
        }
    }

    fn fail_all(&self, message: impl Into<String>) {
        self.accepting
            .store(false, std::sync::atomic::Ordering::Release);
        let message = message.into();
        if let Ok(mut current) = self.current.lock() {
            if let Some(ack) = current.take() {
                ack.complete(Err(message.clone()));
            }
        }
        if let Ok(mut pending) = self.pending.lock() {
            for ack in pending.drain(..) {
                ack.complete(Err(message.clone()));
            }
        }
    }
}

fn signal_forwarder_done(context: &ResultPushContext) {
    if let Ok(mut done) = context.forwarder_done.lock() {
        *done = true;
        context.forwarder_done_cv.notify_all();
    }
}

/// Callback invoked by the plugin's forwarder task to receive the next QueryResult.
/// Blocks until a result is available. Returns null on channel close (shutdown).
///
/// The control block is a versioned request/acknowledgement protocol. The
/// forwarder requests one result at a time and then acknowledges the same item
/// after awaiting the plugin's `Reaction::enqueue_query_result()` callback.
///
/// Wrapped in `catch_unwind` because this is `extern "C"` — panics unwinding
/// across the FFI boundary are undefined behavior.
extern "C" fn result_push_callback(
    ctx: *mut c_void,
    control: *const FfiResultPushControl,
) -> *mut c_void {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        result_push_callback_inner(ctx, control)
    }))
    .unwrap_or_else(|_| {
        // On panic, signal done so drop() doesn't deadlock
        let context = unsafe { &*(ctx as *const ResultPushContext) };
        context.fail_all("Reaction result forwarder panicked");
        signal_forwarder_done(context);
        std::ptr::null_mut()
    })
}

/// Frees a serialized `QueryResult` byte buffer produced by the host in
/// [`result_push_callback_inner`]. Called by the consuming plugin after it has
/// deserialized its own copy (issue #602).
extern "C" fn drop_query_result_bytes(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        unsafe {
            // Rebuild the boxed slice from a *raw* slice pointer — never create a
            // `&mut [u8]` reference to memory we are about to free (that would be UB).
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }
    }
}

fn result_push_callback_inner(
    ctx: *mut c_void,
    control: *const FfiResultPushControl,
) -> *mut c_void {
    let context = unsafe { &*(ctx as *const ResultPushContext) };
    if control.is_null() {
        context.fail_all("Reaction result forwarder used an invalid null control block");
        return std::ptr::null_mut();
    }
    let control = unsafe { &*control };
    if control.version != FFI_RESULT_PUSH_PROTOCOL_VERSION {
        context.fail_all(format!(
            "Reaction result forwarder used unsupported control protocol version {}",
            control.version
        ));
        return std::ptr::null_mut();
    }

    match control.kind {
        FFI_RESULT_PUSH_ACK_OK => {
            context.acknowledge_current(Ok(()));
            return std::ptr::null_mut();
        }
        FFI_RESULT_PUSH_ACK_ERROR => {
            context.acknowledge_current(Err(unsafe { control.error.to_string() }));
            return std::ptr::null_mut();
        }
        FFI_RESULT_PUSH_FORWARDER_EXIT => {
            context.fail_all("Reaction result forwarder exited before acknowledging delivery");
            signal_forwarder_done(context);
            return std::ptr::null_mut();
        }
        FFI_RESULT_PUSH_REQUEST => {}
        _ => {
            context.fail_all(format!(
                "Reaction result forwarder used unknown control kind {}",
                control.kind
            ));
            return std::ptr::null_mut();
        }
    }

    if !context.accepting.load(std::sync::atomic::Ordering::Acquire) {
        return std::ptr::null_mut();
    }

    let guard = context
        .rx
        .lock()
        .expect("result_push_callback lock poisoned");
    if let Some(ref rx) = *guard {
        match rx.recv() {
            Ok(delivery) => {
                if !context.accepting.load(std::sync::atomic::Ordering::Acquire) {
                    delivery.ack.complete(Err(
                        "Reaction result forwarder stopped before delivery".into()
                    ));
                    return std::ptr::null_mut();
                }
                if let Ok(mut current) = context.current.lock() {
                    *current = Some(delivery.ack);
                } else {
                    context.fail_all("Result acknowledgement state is poisoned");
                    return std::ptr::null_mut();
                }
                // Serialize the QueryResult for cross-cdylib transfer (issue #602):
                // never hand the plugin a reinterpreted `repr(Rust)` pointer.
                let bytes = encode_query_result(&delivery.result);
                let payload_len = bytes.len();
                let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
                Box::into_raw(Box::new(FfiQueryResult {
                    payload_ptr,
                    payload_len,
                    payload_drop_fn: Some(drop_query_result_bytes),
                })) as *mut c_void
            }
            Err(_) => {
                // Channel closed — return null so the forwarder breaks.
                // The forwarder will send FORWARDER_EXIT after it has fully exited.
                std::ptr::null_mut()
            }
        }
    } else {
        // rx already taken — return null (forwarder will report its exit)
        std::ptr::null_mut()
    }
}

unsafe impl Send for ReactionProxy {}
unsafe impl Sync for ReactionProxy {}

impl ReactionProxy {
    pub fn new(vtable: ReactionVtable, library: Arc<Library>) -> Self {
        let cached_id = unsafe { (vtable.id_fn)(vtable.state as *const c_void).to_string() };
        let cached_type_name =
            unsafe { (vtable.type_name_fn)(vtable.state as *const c_void).to_string() };
        Self {
            vtable,
            _library: library,
            cached_id,
            cached_type_name,
            _callback_ctx: std::sync::Mutex::new(None),
            result_tx: std::sync::Mutex::new(None),
            _push_ctx: std::sync::Mutex::new(None),
            identity_provider: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl Reaction for ReactionProxy {
    fn id(&self) -> &str {
        &self.cached_id
    }

    fn type_name(&self) -> &str {
        &self.cached_type_name
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let owned = (self.vtable.properties_fn)(self.vtable.state as *const c_void);
        let json_str = unsafe { owned.into_string() };
        match serde_json::from_str(&json_str) {
            Ok(props) => props,
            Err(e) => {
                log::warn!(
                    "Failed to parse plugin properties for '{}': {e}",
                    self.cached_id
                );
                HashMap::new()
            }
        }
    }

    fn query_ids(&self) -> Vec<String> {
        let arr = (self.vtable.query_ids_fn)(self.vtable.state as *const c_void);

        unsafe { arr.into_vec() }
    }

    fn auto_start(&self) -> bool {
        (self.vtable.auto_start_fn)(self.vtable.state as *const c_void)
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        let state_store_vtable = context
            .state_store
            .as_ref()
            .map(|ss| StateStoreVtableBuilder::build(ss.clone()));

        let instance_id_str = context.instance_id.clone();
        let component_id_str = context.reaction_id.clone();

        let instance_id_ffi = FfiStr::from_str(&instance_id_str);
        let component_id_ffi = FfiStr::from_str(&component_id_str);

        let ss_ptr = state_store_vtable
            .map(|v| Box::into_raw(Box::new(v)) as *const _)
            .unwrap_or(std::ptr::null());

        // Create per-instance callback context for this reaction
        let per_instance_ctx = Arc::new(crate::callbacks::InstanceCallbackContext {
            instance_id: instance_id_str.clone(),
            runtime_handle: tokio::runtime::Handle::current(),
            log_registry: drasi_lib::managers::get_or_init_global_registry(),
            update_tx: context.update_tx.clone(),
        });

        // Bug C fix: hand the plugin a strong reference (Arc::into_raw bumps
        // the refcount) so log/lifecycle callbacks emitted late by the plugin
        // (e.g. from inside stop_fn or from internal tasks shutting down) do
        // not deref freed memory. The matching `mem::forget` happens in Drop
        // and intentionally leaks one strong ref per instance.
        let ctx_for_plugin = per_instance_ctx.clone();
        let ctx_ptr = Arc::into_raw(ctx_for_plugin) as *mut c_void;

        if let Ok(mut guard) = self._callback_ctx.lock() {
            *guard = Some(per_instance_ctx);
        }

        let identity_vtable = crate::proxies::identity_resolution::resolve_identity_provider(
            &self.identity_provider,
            context.identity_provider.clone(),
            &format!("Reaction '{}'", self.cached_id),
        )
        .map(crate::identity_bridge::IdentityProviderVtableBuilder::build);

        let ip_ptr: *mut drasi_plugin_sdk::ffi::identity::IdentityProviderVtable = identity_vtable
            .map(|v| Box::into_raw(Box::new(v)))
            .unwrap_or(std::ptr::null_mut());

        let snapshot_fetcher_vtable = context
            .snapshot_fetcher
            .as_ref()
            .map(|sf| SnapshotFetcherVtableBuilder::build(sf.clone()));

        let sf_ptr = snapshot_fetcher_vtable
            .map(|v| Box::into_raw(Box::new(v)) as *const _)
            .unwrap_or(std::ptr::null());

        let ffi_ctx = FfiRuntimeContext {
            instance_id: instance_id_ffi,
            component_id: component_id_ffi,
            state_store: ss_ptr,
            identity_provider: ip_ptr as *const _,
            log_callback: Some(crate::callbacks::instance_log_callback),
            log_ctx: ctx_ptr,
            lifecycle_callback: Some(crate::callbacks::instance_lifecycle_callback),
            lifecycle_ctx: ctx_ptr,
            snapshot_fetcher: sf_ptr,
            wal_provider: std::ptr::null(),
        };

        (self.vtable.initialize_fn)(self.vtable.state, &ffi_ctx as *const FfiRuntimeContext);

        // Reclaim the identity-provider vtable struct we allocated for `ip_ptr`. This is a
        // transient pointer: the plugin SDK (>= 0.10.0) copies the vtable fields by value in
        // `FfiIdentityProviderProxy::new` during `initialize_fn` and never retains `ip_ptr`,
        // so it is safe to free the struct here. Plugins built against SDK < 0.10.0 retained
        // the raw pointer; they are rejected by the loader's exact major.minor version gate
        // (see `validate_plugin_metadata` in `host-sdk/src/loader.rs`), which prevents a
        // use-after-free. This frees only the `IdentityProviderVtable` struct (no `Drop`
        // impl) — the underlying state remains owned by the plugin proxy and is released via
        // `drop_fn` when that proxy is dropped.
        if !ip_ptr.is_null() {
            unsafe {
                drop(Box::from_raw(ip_ptr));
            }
        }
    }

    async fn start(&self) -> anyhow::Result<()> {
        // Set up push-based result channel before starting the reaction
        let (tx, rx) = std::sync::mpsc::sync_channel::<ResultDelivery>(256);
        {
            let mut guard = self.result_tx.lock().expect("result_tx lock poisoned");
            *guard = Some(tx);
        }

        let push_ctx = Arc::new(ResultPushContext {
            rx: std::sync::Mutex::new(Some(rx)),
            pending: std::sync::Mutex::new(Vec::new()),
            current: std::sync::Mutex::new(None),
            accepting: std::sync::atomic::AtomicBool::new(true),
            forwarder_done: std::sync::Mutex::new(false),
            forwarder_done_cv: std::sync::Condvar::new(),
        });
        // Use Arc::as_ptr — the Arc stays alive in _push_ctx for the lifetime of the proxy
        let ctx_ptr = Arc::as_ptr(&push_ctx) as *mut c_void;
        {
            let mut guard = self._push_ctx.lock().expect("_push_ctx lock poisoned");
            *guard = Some(push_ctx);
        }

        // Start the plugin's forwarder task
        (self.vtable.start_result_push_fn)(self.vtable.state, result_push_callback, ctx_ptr);

        // Start the reaction itself
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let start_fn = self.vtable.start_fn;
        let result = std::thread::spawn(move || (start_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        // Fail every accepted delivery before stopping the plugin. The callback
        // context remains alive until the forwarder reports its exit, so an
        // in-flight plugin callback cannot observe freed acknowledgement state.
        if let Ok(guard) = self._push_ctx.lock() {
            if let Some(context) = guard.as_ref() {
                context.fail_all("Reaction stopped before query-result delivery completed");
            }
        }
        {
            let mut guard = self.result_tx.lock().expect("result_tx lock poisoned");
            *guard = None;
        }

        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let stop_fn = self.vtable.stop_fn;
        let result = std::thread::spawn(move || (stop_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    async fn status(&self) -> ComponentStatus {
        let s = (self.vtable.status_fn)(self.vtable.state as *const c_void);
        match s {
            FfiComponentStatus::Starting => ComponentStatus::Starting,
            FfiComponentStatus::Running => ComponentStatus::Running,
            FfiComponentStatus::Stopping => ComponentStatus::Stopping,
            FfiComponentStatus::Stopped => ComponentStatus::Stopped,
            FfiComponentStatus::Reconfiguring => ComponentStatus::Reconfiguring,
            FfiComponentStatus::Error => ComponentStatus::Error,
            FfiComponentStatus::Added => ComponentStatus::Added,
            FfiComponentStatus::Removed => ComponentStatus::Removed,
        }
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        let tx = self
            .result_tx
            .lock()
            .expect("result_tx lock poisoned")
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("Reaction not started — result channel not initialized")
            })?;
        let context = self
            ._push_ctx
            .lock()
            .expect("_push_ctx lock poisoned")
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Reaction result forwarder is not initialized"))?;

        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let ack = Arc::new(ResultAck {
            completion: std::sync::Mutex::new(Some(completion_tx)),
        });
        context.register(ack.clone()).map_err(anyhow::Error::msg)?;

        // Do not block a host runtime worker while the bounded bridge is full:
        // a full bridge is not an accepted delivery and cannot be acknowledged.
        if tx
            .try_send(ResultDelivery {
                result,
                ack: ack.clone(),
            })
            .is_err()
        {
            ack.complete(Err("Reaction result channel is closed or full".into()));
            if let Ok(mut pending) = context.pending.lock() {
                pending.retain(|candidate| !Arc::ptr_eq(candidate, &ack));
            }
        }
        // A suspended enqueue must not keep the bridge open during stop/drop.
        drop(tx);

        completion_rx
            .await
            .map_err(|_| anyhow::anyhow!("Reaction result acknowledgement channel closed"))?
            .map_err(anyhow::Error::msg)
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let deprovision_fn = self.vtable.deprovision_fn;
        let result = std::thread::spawn(move || (deprovision_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    /// Stash a per-instance identity provider that will take precedence over
    /// the runtime-context provider during [`Reaction::initialize`].
    ///
    /// # Timing constraint (FFI reactions only)
    ///
    /// For `ReactionProxy`, the provider must be set **before** the reaction
    /// is added to `DrasiLib` (i.e. before the lifecycle manager calls
    /// `initialize`). There is no FFI hook for late identity-provider
    /// injection — the plugin only receives the provider through
    /// `FfiRuntimeContext` during `initialize_fn`. Calls made after
    /// `initialize` have no effect on the running plugin.
    async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        // See doc comment above for the timing constraint.
        match self.identity_provider.lock() {
            Ok(mut guard) => *guard = Some(provider),
            Err(_) => log::warn!(
                "Reaction '{}': identity_provider mutex is poisoned; provider not set",
                self.cached_id
            ),
        }
    }
    fn is_durable(&self) -> bool {
        (self.vtable.is_durable_fn)(self.vtable.state as *const c_void)
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        (self.vtable.needs_snapshot_on_fresh_start_fn)(self.vtable.state as *const c_void)
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        let ordinal = (self.vtable.default_recovery_policy_fn)(self.vtable.state as *const c_void);
        match ordinal {
            0 => ReactionRecoveryPolicy::Strict,
            1 => ReactionRecoveryPolicy::AutoReset,
            2 => ReactionRecoveryPolicy::AutoSkipGap,
            _ => {
                log::warn!(
                    "Unknown recovery policy ordinal {ordinal} from FFI plugin; defaulting to Strict"
                );
                ReactionRecoveryPolicy::Strict
            }
        }
    }

    async fn bootstrap(
        &self,
        ctx: drasi_lib::reactions::bootstrap_context::BootstrapContext,
    ) -> anyhow::Result<()> {
        // Build the host-side callback context that the plugin's callbacks will
        // use to call back into the host's async BootstrapContext.
        let host_handle = tokio::runtime::Handle::current();
        let ctx = Arc::new(HostBootstrapCallbackCtx { ctx, host_handle });

        let query_id_str = ctx.ctx.query_id.clone();
        let is_reset = ctx.ctx.is_reset;

        // Leak an Arc reference for the callback lifetime. We un-leak after
        // the FFI call returns (see below).
        let ctx_raw = Arc::into_raw(ctx.clone()) as *mut c_void;

        let query_id_ffi = FfiStr::from_str(&query_id_str);

        let ffi_ctx = FfiBootstrapContext {
            query_id: query_id_ffi,
            is_reset,
            callback_ctx: ctx_raw,
            fetch_snapshot_fn: host_bootstrap_fetch_snapshot,
            fetch_outbox_fn: host_bootstrap_fetch_outbox,
            read_checkpoint_fn: host_bootstrap_read_checkpoint,
            write_checkpoint_fn: host_bootstrap_write_checkpoint,
        };

        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let bootstrap_fn = self.vtable.bootstrap_fn;
        let ffi_ctx_ptr = &ffi_ctx as *const FfiBootstrapContext;
        let ffi_ctx_send = SendConstPtr(ffi_ctx_ptr);

        // SAFETY: ffi_ctx lives in this async future's state and we block on
        // join() (not .await), so the pointer is valid for the duration.
        // The join() blocks a tokio worker thread — this is an accepted
        // trade-off per existing FFI conventions. Requires the runtime to have
        // ≥2 worker threads since host_dispatch spawns back on the same runtime.
        let join_result =
            std::thread::spawn(move || (bootstrap_fn)(state.as_ptr(), ffi_ctx_send.as_ptr()))
                .join();

        // Reclaim the leaked Arc reference BEFORE error propagation to prevent
        // memory leaks on thread panic.
        unsafe {
            Arc::from_raw(ctx_raw as *const HostBootstrapCallbackCtx);
        }

        let result =
            join_result.map_err(|_| anyhow::anyhow!("Thread panicked during bootstrap"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }
}

// ============================================================================
// Host-side bootstrap callback support
// ============================================================================

/// Send-safe wrapper for `*const T`.
struct SendConstPtr<T>(*const T);
unsafe impl<T> Send for SendConstPtr<T> {}
impl<T> SendConstPtr<T> {
    fn as_ptr(&self) -> *const T {
        self.0
    }
}

/// Shared state for host-side bootstrap callbacks.
struct HostBootstrapCallbackCtx {
    ctx: drasi_lib::reactions::bootstrap_context::BootstrapContext,
    host_handle: tokio::runtime::Handle,
}

/// Dispatch an async operation to the host's tokio runtime from a blocking
/// thread and wait synchronously for the result.
fn host_dispatch<R: Send + 'static>(
    host_ctx: &HostBootstrapCallbackCtx,
    fut: impl std::future::Future<Output = R> + Send + 'static,
) -> R {
    let (tx, rx) = std::sync::mpsc::sync_channel::<R>(0);
    host_ctx.host_handle.spawn(async move {
        let result = fut.await;
        let _ = tx.send(result);
    });
    rx.recv().expect("host bootstrap dispatch channel dropped")
}

// ---- Snapshot iterator callbacks ----

/// Host-allocated streaming snapshot iterator state.
///
/// Holds a dedicated current-thread tokio runtime and the `SnapshotStream`.
/// Each `next_fn` call pulls exactly one row via `rt.block_on(stream.next())`.
struct SnapshotIteratorState {
    /// `Option` so `drop_fn` can move it to a dedicated thread for safe cleanup.
    rt: Option<tokio::runtime::Runtime>,
    /// `Option` so we can drop the stream before the runtime in `drop_fn`.
    stream: Option<drasi_lib::queries::output_state::SnapshotStream>,
}

extern "C" fn snapshot_iter_next(iter_ctx: *mut c_void) -> drasi_plugin_sdk::ffi::FfiOwnedStr {
    if iter_ctx.is_null() {
        return drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new());
    }
    let state = unsafe { &mut *(iter_ctx as *mut SnapshotIteratorState) };
    let (rt, stream) = match (state.rt.as_ref(), state.stream.as_mut()) {
        (Some(rt), Some(s)) => (rt, s),
        _ => return drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
    };
    match rt.block_on(stream.next_keyed()) {
        Some((sig, row)) => {
            let envelope = drasi_lib::queries::output_state::KeyedSnapshotRow { k: sig, v: row };
            let json = serde_json::to_string(&envelope).unwrap_or_else(|_| "null".into());
            drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(json)
        }
        None => drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
    }
}

extern "C" fn snapshot_iter_drop(iter_ctx: *mut c_void) {
    if !iter_ctx.is_null() {
        let mut state = unsafe { Box::from_raw(iter_ctx as *mut SnapshotIteratorState) };
        // Drop the stream first to release async resources.
        state.stream = None;
        // Move the runtime to a dedicated thread for safe cleanup — dropping a
        // tokio Runtime from within an async context panics.
        if let Some(rt) = state.rt.take() {
            std::thread::spawn(move || drop(rt));
        }
    }
}

fn make_error_snapshot_response(msg: String) -> FfiSnapshotIteratorResponse {
    FfiSnapshotIteratorResponse {
        iterator: FfiSnapshotIterator {
            iter_ctx: std::ptr::null_mut(),
            next_fn: snapshot_iter_next,
            drop_fn: snapshot_iter_drop,
        },
        as_of_sequence: 0,
        config_hash: 0,
        error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(msg),
    }
}

extern "C" fn host_bootstrap_fetch_snapshot(ctx: *mut c_void) -> FfiSnapshotIteratorResponse {
    if ctx.is_null() {
        return make_error_snapshot_response("null callback context".into());
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let host_ctx = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };

        // Phase 1: Fetch the SnapshotStream on the host runtime.
        let result = host_dispatch(host_ctx, {
            let ctx_ref = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };
            async move { ctx_ref.ctx.fetch_snapshot().await }
        });

        match result {
            Ok(snapshot) => {
                let as_of_sequence = snapshot.as_of_sequence;
                let config_hash = snapshot.config_hash;

                // Phase 2: Create a lightweight runtime for lazy iteration.
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        return make_error_snapshot_response(format!(
                            "failed to build iterator runtime: {e}"
                        ));
                    }
                };

                let iter_state = Box::new(SnapshotIteratorState {
                    rt: Some(rt),
                    stream: Some(snapshot),
                });
                let iter_ctx = Box::into_raw(iter_state) as *mut c_void;

                FfiSnapshotIteratorResponse {
                    iterator: FfiSnapshotIterator {
                        iter_ctx,
                        next_fn: snapshot_iter_next,
                        drop_fn: snapshot_iter_drop,
                    },
                    as_of_sequence,
                    config_hash,
                    error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
                }
            }
            Err(e) => make_error_snapshot_response(format!("{e}")),
        }
    }))
    .unwrap_or_else(|_| make_error_snapshot_response("panic in host bootstrap callback".into()))
}

// ---- Outbox iterator callbacks ----

/// Host-allocated streaming outbox iterator state.
struct OutboxIteratorState {
    /// `Option` so `drop_fn` can move it to a dedicated thread for safe cleanup.
    rt: Option<tokio::runtime::Runtime>,
    /// `Option` so we can drop the stream before the runtime in `drop_fn`.
    stream: Option<drasi_lib::queries::output_state::OutboxStream>,
}

extern "C" fn outbox_iter_next(iter_ctx: *mut c_void) -> drasi_plugin_sdk::ffi::FfiOwnedStr {
    if iter_ctx.is_null() {
        return drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new());
    }
    let state = unsafe { &mut *(iter_ctx as *mut OutboxIteratorState) };
    let (rt, stream) = match (state.rt.as_ref(), state.stream.as_mut()) {
        (Some(rt), Some(s)) => (rt, s),
        _ => return drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
    };
    use tokio_stream::StreamExt;
    match rt.block_on(stream.next()) {
        Some(entry) => {
            let json = serde_json::to_string(entry.as_ref()).unwrap_or_else(|_| "null".into());
            drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(json)
        }
        None => drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
    }
}

extern "C" fn outbox_iter_drop(iter_ctx: *mut c_void) {
    if !iter_ctx.is_null() {
        let mut state = unsafe { Box::from_raw(iter_ctx as *mut OutboxIteratorState) };
        state.stream = None;
        if let Some(rt) = state.rt.take() {
            std::thread::spawn(move || drop(rt));
        }
    }
}

fn make_error_outbox_response(
    msg: String,
    latest_sequence: u64,
    config_hash: u64,
) -> FfiOutboxIteratorResponse {
    FfiOutboxIteratorResponse {
        iterator: FfiOutboxIterator {
            iter_ctx: std::ptr::null_mut(),
            next_fn: outbox_iter_next,
            drop_fn: outbox_iter_drop,
        },
        latest_sequence,
        config_hash,
        error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(msg),
    }
}

extern "C" fn host_bootstrap_fetch_outbox(
    ctx: *mut c_void,
    after_sequence: u64,
) -> FfiOutboxIteratorResponse {
    if ctx.is_null() {
        return make_error_outbox_response("null callback context".into(), 0, 0);
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let host_ctx = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };

        // Phase 1: Fetch the OutboxStream on the host runtime.
        let result = host_dispatch(host_ctx, {
            let ctx_ref = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };
            async move { ctx_ref.ctx.fetch_outbox(after_sequence).await }
        });

        match result {
            Ok(outbox) => {
                let latest_sequence = outbox.latest_sequence;
                let config_hash = outbox.config_hash;

                // Phase 2: Create a lightweight runtime for lazy iteration.
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        return make_error_outbox_response(
                            format!("failed to build iterator runtime: {e}"),
                            latest_sequence,
                            config_hash,
                        );
                    }
                };

                let iter_state = Box::new(OutboxIteratorState {
                    rt: Some(rt),
                    stream: Some(outbox),
                });
                let iter_ctx = Box::into_raw(iter_state) as *mut c_void;

                FfiOutboxIteratorResponse {
                    iterator: FfiOutboxIterator {
                        iter_ctx,
                        next_fn: outbox_iter_next,
                        drop_fn: outbox_iter_drop,
                    },
                    latest_sequence,
                    config_hash,
                    error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
                }
            }
            Err(e) => {
                let (err_msg, gap_latest, gap_hash) = match e {
                    drasi_lib::queries::output_state::FetchError::OutboxGap(gap) => (
                        format!("OutboxGap:{}", gap.earliest_available),
                        gap.latest_sequence,
                        gap.config_hash,
                    ),
                    other => (format!("{other}"), 0, 0),
                };
                make_error_outbox_response(err_msg, gap_latest, gap_hash)
            }
        }
    }))
    .unwrap_or_else(|_| make_error_outbox_response("panic in host bootstrap callback".into(), 0, 0))
}

extern "C" fn host_bootstrap_read_checkpoint(ctx: *mut c_void) -> FfiCheckpointResult {
    if ctx.is_null() {
        return FfiCheckpointResult {
            found: false,
            checkpoint: FfiCheckpoint {
                sequence: 0,
                config_hash: 0,
            },
            error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string("null callback context".into()),
        };
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let host_ctx = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };
        let result = host_dispatch(host_ctx, {
            let ctx_ref = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };
            async move { ctx_ref.ctx.read_checkpoint().await }
        });
        match result {
            Ok(Some(cp)) => FfiCheckpointResult {
                found: true,
                checkpoint: FfiCheckpoint {
                    sequence: cp.sequence,
                    config_hash: cp.config_hash,
                },
                error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
            },
            Ok(None) => FfiCheckpointResult {
                found: false,
                checkpoint: FfiCheckpoint {
                    sequence: 0,
                    config_hash: 0,
                },
                error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(String::new()),
            },
            Err(e) => FfiCheckpointResult {
                found: false,
                checkpoint: FfiCheckpoint {
                    sequence: 0,
                    config_hash: 0,
                },
                error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(format!("{e}")),
            },
        }
    }))
    .unwrap_or_else(|_| FfiCheckpointResult {
        found: false,
        checkpoint: FfiCheckpoint {
            sequence: 0,
            config_hash: 0,
        },
        error: drasi_plugin_sdk::ffi::FfiOwnedStr::from_string(
            "panic in host bootstrap callback".into(),
        ),
    })
}

extern "C" fn host_bootstrap_write_checkpoint(
    ctx: *mut c_void,
    checkpoint: FfiCheckpoint,
) -> FfiResult {
    if ctx.is_null() {
        return FfiResult::err("null callback context".into());
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let host_ctx = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };
        let cp = drasi_lib::reactions::ReactionCheckpoint {
            sequence: checkpoint.sequence,
            config_hash: checkpoint.config_hash,
        };
        let result = host_dispatch(host_ctx, {
            let ctx_ref = unsafe { &*(ctx as *const HostBootstrapCallbackCtx) };
            async move { ctx_ref.ctx.write_checkpoint(&cp).await }
        });
        match result {
            Ok(()) => FfiResult::ok(),
            Err(e) => FfiResult::err(format!("{e}")),
        }
    }))
    .unwrap_or_else(|_| FfiResult::err("panic in host bootstrap callback".into()))
}

impl Drop for ReactionProxy {
    fn drop(&mut self) {
        // Close the result channel sender to unblock the forwarder's callback.
        // The callback's rx.recv() will return Err, causing it to return null.
        // The forwarder then reports FORWARDER_EXIT to signal forwarder_done.
        if let Ok(mut guard) = self.result_tx.lock() {
            *guard = None;
        }
        if let Ok(guard) = self._push_ctx.lock() {
            if let Some(context) = guard.as_ref() {
                context.fail_all("Reaction proxy dropped before query-result delivery completed");
            }
        }
        // Do NOT drop the receiver here. Leaving it in place avoids racing a
        // callback that is currently holding `context.rx.lock()`.

        // Wait for the forwarder task to fully exit its processing loop.
        //
        // Safety argument: the forwarder sends a sentinel callback AFTER
        // breaking out of its loop. At that point, all enqueue_query_result()
        // calls have finished and the forwarder will NOT access the
        // ReactionWrapper again. Therefore, after this signal fires,
        // it is safe to free the ReactionWrapper.
        let forwarder_exited = if let Ok(guard) = self._push_ctx.lock() {
            if let Some(ref ctx) = *guard {
                let done = ctx.forwarder_done.lock().expect("forwarder_done lock");
                let (guard, timeout) = ctx
                    .forwarder_done_cv
                    .wait_timeout_while(done, std::time::Duration::from_secs(5), |done| !*done)
                    .expect("forwarder_done condvar wait");
                !timeout.timed_out() && *guard
            } else {
                true // No push context → forwarder was never started
            }
        } else {
            false // Lock poisoned
        };

        if forwarder_exited {
            // Safe to free the ReactionWrapper — forwarder won't access it.
            let drop_fn = self.vtable.drop_fn;
            let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
            super::drop_worker::execute_drop_fn(drop_fn, state);
        } else {
            // Timeout or error — leak the ReactionWrapper to prevent UAF.
            // Memory leak is preferable to undefined behavior.
            log::warn!(
                "ReactionProxy::drop: forwarder did not exit within timeout; \
                 leaking ReactionWrapper to prevent use-after-free"
            );
        }

        // Leak the push context Arc on the timeout path — the forwarder's
        // spawn_blocking callback may still reference it. On the success path
        // this is unnecessary but harmless, and keeps the logic simple.
        if let Ok(mut guard) = self._push_ctx.lock() {
            if let Some(ctx) = guard.take() {
                std::mem::forget(ctx);
            }
        }

        // Bug C fix: leak the per-instance callback context Arc unconditionally.
        // The strong reference handed to the plugin via `Arc::into_raw` in
        // initialize() is never reclaimed — late log/lifecycle callbacks
        // emitted by the plugin (during stop_fn or from internal tasks) must
        // still find a valid pointer. The cdylib itself is intentionally
        // leaked process-wide (see host-sdk/src/loader.rs), so this small
        // per-instance Arc leak is the price of closing the late-callback
        // UAF window.
        if let Ok(mut guard) = self._callback_ctx.lock() {
            if let Some(ctx) = guard.take() {
                std::mem::forget(ctx);
            }
        }
    }
}

// ============================================================================
// ReactionPluginProxy — wraps ReactionPluginVtable into ReactionPluginDescriptor
// ============================================================================

/// Wraps a `ReactionPluginVtable` (factory) into a `ReactionPluginDescriptor`.
pub struct ReactionPluginProxy {
    vtable: ReactionPluginVtable,
    library: Arc<Library>,
    cached_kind: String,
    cached_config_version: String,
    cached_config_schema_name: String,
    plugin_id: String,
}

unsafe impl Send for ReactionPluginProxy {}
unsafe impl Sync for ReactionPluginProxy {}

impl ReactionPluginProxy {
    pub fn new(vtable: ReactionPluginVtable, library: Arc<Library>) -> Self {
        let cached_kind = unsafe { (vtable.kind_fn)(vtable.state as *const c_void).to_string() };
        let cached_config_version =
            unsafe { (vtable.config_version_fn)(vtable.state as *const c_void).to_string() };
        let cached_config_schema_name =
            unsafe { (vtable.config_schema_name_fn)(vtable.state as *const c_void).to_string() };
        Self {
            vtable,
            library,
            cached_kind,
            cached_config_version,
            cached_config_schema_name,
            plugin_id: String::new(),
        }
    }

    /// The unique identifier of the plugin that provided this descriptor.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Set the plugin identity for this descriptor.
    pub fn set_plugin_id(&mut self, id: String) {
        self.plugin_id = id;
    }
}

#[async_trait]
impl ReactionPluginDescriptor for ReactionPluginProxy {
    fn kind(&self) -> &str {
        &self.cached_kind
    }

    fn config_version(&self) -> &str {
        &self.cached_config_version
    }

    fn config_schema_json(&self) -> String {
        unsafe {
            (self.vtable.config_schema_json_fn)(self.vtable.state as *const c_void).into_string()
        }
    }

    fn config_schema_name(&self) -> &str {
        &self.cached_config_schema_name
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let config_str = serde_json::to_string(config_json)?;
        let query_ids_str = serde_json::to_string(&query_ids)?;
        let id_ffi = FfiStr::from_str(id);
        let query_ids_ffi = FfiStr::from_str(&query_ids_str);
        let config_ffi = FfiStr::from_str(&config_str);

        let state = self.vtable.state;
        let create_fn = self.vtable.create_reaction_fn;
        let result = (create_fn)(state, id_ffi, query_ids_ffi, config_ffi, auto_start);

        let vtable_ptr = unsafe {
            result
                .into_result::<ReactionVtable>()
                .map_err(|msg| anyhow::anyhow!("{msg}"))?
        };

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for reaction '{id}'"
            ));
        }

        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(ReactionProxy::new(vtable, self.library.clone())))
    }
}

impl Drop for ReactionPluginProxy {
    fn drop(&mut self) {
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);
    }
}

#[cfg(test)]
mod result_push_ack_tests {
    use super::*;

    fn context() -> (
        Arc<ResultPushContext>,
        std::sync::mpsc::SyncSender<ResultDelivery>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        (
            Arc::new(ResultPushContext {
                rx: std::sync::Mutex::new(Some(rx)),
                pending: std::sync::Mutex::new(Vec::new()),
                current: std::sync::Mutex::new(None),
                accepting: std::sync::atomic::AtomicBool::new(true),
                forwarder_done: std::sync::Mutex::new(false),
                forwarder_done_cv: std::sync::Condvar::new(),
            }),
            tx,
        )
    }

    fn queue_delivery(
        context: &ResultPushContext,
        tx: &std::sync::mpsc::SyncSender<ResultDelivery>,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let ack = Arc::new(ResultAck {
            completion: std::sync::Mutex::new(Some(completion_tx)),
        });
        context.register(ack.clone()).expect("register delivery");
        tx.send(ResultDelivery {
            result: drasi_lib::channels::QueryResult::new(
                "query".into(),
                0,
                chrono::Utc::now(),
                Vec::new(),
                HashMap::new(),
            ),
            ack,
        })
        .expect("queue delivery");
        completion_rx
    }

    fn callback(context: &ResultPushContext, kind: u32, error: &str) -> *mut c_void {
        let control = FfiResultPushControl {
            version: FFI_RESULT_PUSH_PROTOCOL_VERSION,
            kind,
            error: FfiStr::from_str(error),
        };
        result_push_callback(
            context as *const ResultPushContext as *mut c_void,
            &control as *const FfiResultPushControl,
        )
    }

    unsafe fn free_result(ptr: *mut c_void) {
        let result = unsafe { Box::from_raw(ptr as *mut FfiQueryResult) };
        if let Some(drop_payload) = result.payload_drop_fn {
            drop_payload(result.payload_ptr as *mut u8, result.payload_len);
        }
    }

    #[tokio::test]
    async fn acknowledgement_completes_the_specific_delivery() {
        let (context, tx) = context();
        let completion = queue_delivery(&context, &tx);

        let result = callback(&context, FFI_RESULT_PUSH_REQUEST, "");
        assert!(
            !result.is_null(),
            "request must receive a serialized result"
        );
        unsafe { free_result(result) };
        callback(&context, FFI_RESULT_PUSH_ACK_OK, "");

        assert_eq!(completion.await.expect("ack channel"), Ok(()));
    }

    #[tokio::test]
    async fn delivery_stays_pending_until_callback_acknowledges_it() {
        let (context, tx) = context();
        let mut completion = queue_delivery(&context, &tx);

        let result = callback(&context, FFI_RESULT_PUSH_REQUEST, "");
        assert!(
            !result.is_null(),
            "request must receive a serialized result"
        );
        unsafe { free_result(result) };
        assert!(
            matches!(
                completion.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "queue receipt must not complete the host delivery"
        );

        callback(&context, FFI_RESULT_PUSH_ACK_OK, "");
        assert_eq!(completion.await.expect("ack channel"), Ok(()));
    }

    #[tokio::test]
    async fn callback_failure_propagates_its_exact_message() {
        let (context, tx) = context();
        let completion = queue_delivery(&context, &tx);

        let result = callback(&context, FFI_RESULT_PUSH_REQUEST, "");
        unsafe { free_result(result) };
        callback(
            &context,
            FFI_RESULT_PUSH_ACK_ERROR,
            "GitHub update failed: forbidden",
        );

        assert_eq!(
            completion.await.expect("ack channel"),
            Err("GitHub update failed: forbidden".into())
        );
    }

    #[tokio::test]
    async fn forwarder_exit_fails_pending_delivery_and_signals_drop_safety() {
        let (context, tx) = context();
        let completion = queue_delivery(&context, &tx);

        let result = callback(&context, FFI_RESULT_PUSH_REQUEST, "");
        unsafe { free_result(result) };
        callback(&context, FFI_RESULT_PUSH_FORWARDER_EXIT, "");

        let error = completion
            .await
            .expect("ack channel")
            .expect_err("forwarder exit must fail delivery");
        assert!(error.contains("forwarder exited"));
        assert!(*context.forwarder_done.lock().expect("forwarder done"));
    }
}
