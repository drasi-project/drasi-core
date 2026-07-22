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

//! Host-side proxy for Source and SourcePluginDescriptor.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::channels::events::SubscriptionResponse;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::identity::IdentityProvider;
use drasi_lib::schema::SourceSchema;
use drasi_lib::sources::Source;
use drasi_lib::{ComponentStatus, DispatchMode, SourceRuntimeContext};
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;
use drasi_plugin_sdk::ffi::payload::encode_query_result;
use drasi_plugin_sdk::ffi::{
    FfiComponentStatus, FfiDispatchMode, FfiQueryResult, FfiResultPushCallbackFn,
    FfiRuntimeContext, FfiStr, SourcePluginVtable, SourceVtable,
};
use libloading::Library;

use super::change_receiver::{BootstrapReceiverProxy, ChangeReceiverProxy};
use crate::state_store_bridge::StateStoreVtableBuilder;

/// Host-side async executor for FFI vtable operations.
///
/// Runs the pinned future on a new OS thread with a current-thread tokio runtime.
/// This avoids nesting issues with the host's multi-thread runtime.
extern "C" fn host_executor(future_ptr: *mut c_void) -> *mut c_void {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Wrap the raw pointer to make it Send-safe for std::thread::spawn
        let send_ptr = drasi_plugin_sdk::ffi::SendMutPtr(future_ptr);
        let result = std::thread::spawn(move || {
            let boxed_future = unsafe {
                Box::from_raw(send_ptr.as_ptr()
                    as *mut std::pin::Pin<Box<dyn std::future::Future<Output = *mut c_void>>>)
            };
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return drasi_plugin_sdk::ffi::SendMutPtr(std::ptr::null_mut()),
            };
            // Wrap the result in SendMutPtr to satisfy Send bound
            drasi_plugin_sdk::ffi::SendMutPtr(rt.block_on(*boxed_future))
        })
        .join()
        .map(|p| p.as_ptr())
        .unwrap_or(std::ptr::null_mut());
        result
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Context for the push-based query-result callback used to feed a
/// query-consuming source's `enqueue_query_result` across the FFI boundary.
///
/// Mirrors the equivalent machinery in the reaction proxy: the host holds the
/// receiving end of a channel; the plugin's forwarder task pulls each
/// `QueryResult` via [`result_push_callback`], which blocks on `rx.recv()`.
struct ResultPushContext {
    rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<drasi_lib::channels::QueryResult>>>,
    /// Signaled when the plugin-side forwarder task has fully exited its loop and
    /// will no longer access the source wrapper. The forwarder signals this by
    /// calling the callback one final time with a non-null sentinel parameter.
    forwarder_done: std::sync::Mutex<bool>,
    forwarder_done_cv: std::sync::Condvar,
}

fn signal_forwarder_done(context: &ResultPushContext) {
    if let Ok(mut done) = context.forwarder_done.lock() {
        *done = true;
        context.forwarder_done_cv.notify_all();
    }
}

/// Frees a serialized `QueryResult` byte buffer produced by the host in
/// [`result_push_callback_inner`]. Called by the consuming plugin after it has
/// deserialized its own copy.
extern "C" fn drop_query_result_bytes(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }
    }
}

/// Callback invoked by the plugin's forwarder task to receive the next
/// `QueryResult`. Blocks until a result is available. Returns null on channel
/// close (shutdown). A non-null `sentinel` signals that the forwarder has fully
/// exited and will not access the source wrapper again.
///
/// Wrapped in `catch_unwind` because this is `extern "C"` — panics unwinding
/// across the FFI boundary are undefined behavior.
extern "C" fn result_push_callback(ctx: *mut c_void, sentinel: *mut c_void) -> *mut c_void {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        result_push_callback_inner(ctx, sentinel)
    }))
    .unwrap_or_else(|_| {
        let context = unsafe { &*(ctx as *const ResultPushContext) };
        signal_forwarder_done(context);
        std::ptr::null_mut()
    })
}

fn result_push_callback_inner(ctx: *mut c_void, sentinel: *mut c_void) -> *mut c_void {
    let context = unsafe { &*(ctx as *const ResultPushContext) };

    // Sentinel call: the forwarder has exited its loop.
    if !sentinel.is_null() {
        signal_forwarder_done(context);
        return std::ptr::null_mut();
    }

    let guard = context
        .rx
        .lock()
        .expect("result_push_callback lock poisoned");
    if let Some(ref rx) = *guard {
        match rx.recv() {
            Ok(result) => {
                // Serialize the QueryResult for cross-cdylib transfer — never hand
                // the plugin a reinterpreted `repr(Rust)` pointer (issue #602).
                let bytes = encode_query_result(&result);
                let payload_len = bytes.len();
                let payload_ptr = Box::into_raw(bytes.into_boxed_slice()) as *const u8;
                Box::into_raw(Box::new(FfiQueryResult {
                    payload_ptr,
                    payload_len,
                    payload_drop_fn: Some(drop_query_result_bytes),
                })) as *mut c_void
            }
            // Channel closed — return null so the forwarder breaks and then sends
            // the sentinel.
            Err(_) => std::ptr::null_mut(),
        }
    } else {
        std::ptr::null_mut()
    }
}

/// Wraps a `SourceVtable` into a DrasiLib `Source` trait implementation.
///
/// The host creates this proxy when the plugin factory produces a `SourceVtable`.
/// All trait method calls dispatch through the vtable function pointers.
pub struct SourceProxy {
    vtable: SourceVtable,
    _library: Arc<Library>,
    cached_id: String,
    cached_type_name: String,
    /// Keeps the per-instance callback context alive for the lifetime of this proxy.
    _callback_ctx: std::sync::Mutex<Option<Arc<crate::callbacks::InstanceCallbackContext>>>,
    /// Channel for push-based query-result delivery. Created on start, closed on
    /// stop/drop. Only used by query-consuming sources.
    result_tx:
        std::sync::Mutex<Option<std::sync::mpsc::SyncSender<drasi_lib::channels::QueryResult>>>,
    /// Keeps the push callback context alive for the lifetime of the forwarder.
    _push_ctx: std::sync::Mutex<Option<Arc<ResultPushContext>>>,
    /// Per-source identity provider set programmatically via
    /// [`Source::set_identity_provider`]. When present, it takes precedence over
    /// any instance-wide provider supplied via
    /// [`SourceRuntimeContext::identity_provider`] during [`Source::initialize`].
    identity_provider: std::sync::Mutex<Option<Arc<dyn IdentityProvider>>>,
}

unsafe impl Send for SourceProxy {}
unsafe impl Sync for SourceProxy {}

impl SourceProxy {
    pub fn new(vtable: SourceVtable, library: Arc<Library>) -> Self {
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
impl Source for SourceProxy {
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

    fn dispatch_mode(&self) -> DispatchMode {
        let mode = (self.vtable.dispatch_mode_fn)(self.vtable.state as *const c_void);
        match mode {
            FfiDispatchMode::Channel => DispatchMode::Channel,
            FfiDispatchMode::Broadcast => DispatchMode::Broadcast,
        }
    }

    fn auto_start(&self) -> bool {
        (self.vtable.auto_start_fn)(self.vtable.state as *const c_void)
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        // Safety: both the raw function-pointer call and `into_string()` (which calls
        // `String::from_raw_parts`) require unsafe.
        let json = unsafe {
            (self.vtable.describe_schema_fn)(self.vtable.state as *const c_void).into_string()
        };

        match serde_json::from_str(&json) {
            Ok(schema) => schema,
            Err(e) => {
                log::warn!(
                    "Failed to parse plugin schema for '{}': {e}",
                    self.cached_id
                );
                None
            }
        }
    }

    async fn start(&self) -> anyhow::Result<()> {
        // For query-consuming sources, set up push-based result delivery before
        // starting the source, so query results forwarded by the host reach the
        // plugin's `enqueue_query_result` across the FFI boundary. Ordinary
        // sources (no subscribed queries) skip this entirely.
        if !self.subscribed_query_ids().is_empty() {
            let (tx, rx) = std::sync::mpsc::sync_channel::<drasi_lib::channels::QueryResult>(256);
            {
                let mut guard = self.result_tx.lock().expect("result_tx lock poisoned");
                *guard = Some(tx);
            }
            let push_ctx = Arc::new(ResultPushContext {
                rx: std::sync::Mutex::new(Some(rx)),
                forwarder_done: std::sync::Mutex::new(false),
                forwarder_done_cv: std::sync::Condvar::new(),
            });
            // Use Arc::as_ptr — the Arc stays alive in _push_ctx for the proxy's life.
            let ctx_ptr = Arc::as_ptr(&push_ctx) as *mut c_void;
            {
                let mut guard = self._push_ctx.lock().expect("_push_ctx lock poisoned");
                *guard = Some(push_ctx);
            }
            (self.vtable.start_result_push_fn)(self.vtable.state, result_push_callback, ctx_ptr);
        }

        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let start_fn = self.vtable.start_fn;
        let result = std::thread::spawn(move || (start_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        // Close the result channel sender to unblock the plugin forwarder's
        // callback (its rx.recv() returns Err, the callback returns null, and the
        // forwarder breaks out of its loop). The forwarder task is joined in Drop
        // via the sentinel signal before the wrapper is freed.
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

    fn subscribed_query_ids(&self) -> Vec<String> {
        let arr = (self.vtable.subscribed_query_ids_fn)(self.vtable.state as *const c_void);
        unsafe { arr.into_vec() }
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        let guard = self.result_tx.lock().expect("result_tx lock poisoned");
        if let Some(ref tx) = *guard {
            tx.send(result)
                .map_err(|_| anyhow::anyhow!("Result channel closed"))?;
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Source not started — result channel not initialized"
            ))
        }
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

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        let nodes_json = serde_json::to_string(&settings.nodes)?;
        let relations_json = serde_json::to_string(&settings.relations)?;

        let source_id_ffi = FfiStr::from_str(&settings.source_id);
        let query_id_ffi = FfiStr::from_str(&settings.query_id);
        let nodes_ffi = FfiStr::from_str(&nodes_json);
        let relations_ffi = FfiStr::from_str(&relations_json);
        let enable_bootstrap = settings.enable_bootstrap;

        // Pass resume_from position bytes across FFI (null ptr + 0 len if None)
        let (resume_from_ptr, resume_from_len) = match &settings.resume_from {
            Some(bytes) => (bytes.as_ptr(), bytes.len() as u32),
            None => (std::ptr::null(), 0u32),
        };

        let resp_ptr = (self.vtable.subscribe_fn)(
            self.vtable.state,
            source_id_ffi,
            enable_bootstrap,
            query_id_ffi,
            nodes_ffi,
            relations_ffi,
            resume_from_ptr,
            resume_from_len,
            settings.request_position_handle,
        );

        if resp_ptr.is_null() {
            return Err(anyhow::anyhow!("Subscribe returned null"));
        }

        let ffi_resp = unsafe { *Box::from_raw(resp_ptr) };
        let query_id = unsafe { ffi_resp.query_id.into_string() };
        let source_id = unsafe { ffi_resp.source_id.into_string() };

        let receiver = if ffi_resp.receiver.is_null() {
            return Err(anyhow::anyhow!("Subscribe returned null receiver"));
        } else {
            let ffi_cr = unsafe { *Box::from_raw(ffi_resp.receiver) };
            // Run on a dedicated thread to avoid initializing plugin TLS
            // on the caller's thread. On macOS, plugin TLS destructors
            // can deadlock with the still-running plugin runtime during
            // thread exit.
            let proxy = std::thread::spawn(move || ChangeReceiverProxy::new(ffi_cr))
                .join()
                .map_err(|_| anyhow::anyhow!("ChangeReceiverProxy::new thread panicked"))?;
            Box::new(proxy)
                as Box<
                    dyn drasi_lib::channels::ChangeReceiver<
                        drasi_lib::channels::events::SourceEventWrapper,
                    >,
                >
        };

        let bootstrap_receiver = if ffi_resp.bootstrap_receiver.is_null() {
            None
        } else {
            let ffi_br = unsafe { *Box::from_raw(ffi_resp.bootstrap_receiver) };
            // Same thread isolation for bootstrap receiver
            let proxy = std::thread::spawn(move || BootstrapReceiverProxy::new(ffi_br))
                .join()
                .map_err(|_| anyhow::anyhow!("BootstrapReceiverProxy::new thread panicked"))?;
            Some(proxy.into_mpsc_receiver())
        };

        // Reconstruct position_handle from Arc::into_raw pointer
        let position_handle = if ffi_resp.position_handle_ptr.is_null() {
            None
        } else {
            // Safety: the plugin side did Arc::into_raw(arc) which transfers one
            // ref-count. We reconstruct the Arc without incrementing. The plugin's
            // SourceBase holds another clone, so the AtomicU64 stays alive until
            // both sides drop their Arcs.
            Some(unsafe {
                std::sync::Arc::from_raw(
                    ffi_resp.position_handle_ptr as *const std::sync::atomic::AtomicU64,
                )
            })
        };

        // Reconstruct bootstrap_result_receiver via push-based callback
        let bootstrap_result_receiver = if ffi_resp.bootstrap_result_receiver.is_null() {
            None
        } else {
            let ffi_brr = unsafe { *Box::from_raw(ffi_resp.bootstrap_result_receiver) };
            let (host_tx, host_rx) = tokio::sync::oneshot::channel::<
                anyhow::Result<drasi_lib::bootstrap::BootstrapResult>,
            >();

            // Wrap sender in Mutex<Option<...>> so duplicate callbacks are
            // harmless (second call sees None and is a no-op).
            struct BootstrapResultCtx {
                tx: std::sync::Mutex<
                    Option<
                        tokio::sync::oneshot::Sender<
                            anyhow::Result<drasi_lib::bootstrap::BootstrapResult>,
                        >,
                    >,
                >,
            }

            // Callback the plugin will invoke when bootstrap result is ready
            extern "C" fn host_bootstrap_result_callback(
                ctx: *mut std::ffi::c_void,
                result: *mut drasi_plugin_sdk::ffi::FfiBootstrapResult,
            ) {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if ctx.is_null() {
                        return;
                    }
                    let wrapper = unsafe { Box::from_raw(ctx as *mut BootstrapResultCtx) };
                    let tx = match wrapper.tx.lock() {
                        Ok(mut guard) => guard.take(),
                        Err(_) => None,
                    };
                    let Some(tx) = tx else {
                        return;
                    };
                    if result.is_null() {
                        let _ = tx.send(Err(anyhow::anyhow!(
                            "Bootstrap result receiver dropped without result"
                        )));
                        return;
                    }
                    let ffi_result = unsafe { *Box::from_raw(result) };
                    if ffi_result.event_count < 0 {
                        let _ = tx.send(Err(anyhow::anyhow!(
                            "Bootstrap failed with code {}",
                            ffi_result.event_count
                        )));
                        return;
                    }
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
                    let _ = tx.send(Ok(drasi_lib::bootstrap::BootstrapResult {
                        event_count: ffi_result.event_count as usize,
                        source_position,
                    }));
                }));
            }

            let ctx_wrapper = Box::new(BootstrapResultCtx {
                tx: std::sync::Mutex::new(Some(host_tx)),
            });
            let ctx = Box::into_raw(ctx_wrapper) as *mut std::ffi::c_void;
            (ffi_brr.start_fn)(ffi_brr.state, host_bootstrap_result_callback, ctx);
            // Safe to call drop_fn: the plugin state is Arc-based, so the
            // spawned task holds its own clone. Dropping the FFI handle's Arc
            // reference won't free the state while the task is alive.
            (ffi_brr.drop_fn)(ffi_brr.state);

            Some(host_rx)
        };

        Ok(SubscriptionResponse {
            query_id,
            source_id,
            receiver,
            bootstrap_receiver,
            position_handle,
            bootstrap_result_receiver,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn supports_replay(&self) -> bool {
        (self.vtable.supports_replay_fn)(self.vtable.state)
    }

    async fn remove_position_handle(&self, query_id: &str) {
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let remove_fn = self.vtable.remove_position_handle_fn;
        let qid = query_id.to_string();
        let result = std::thread::spawn(move || {
            let ffi_qid = drasi_plugin_sdk::ffi::FfiStr::from_str(&qid);
            (remove_fn)(state.as_ptr(), ffi_qid)
        })
        .join()
        .map(|r| unsafe { r.into_result() });

        match result {
            Ok(Ok(())) => {
                log::debug!("SourceProxy::remove_position_handle('{query_id}') completed via FFI");
            }
            Ok(Err(e)) => {
                log::warn!("SourceProxy::remove_position_handle('{query_id}') FFI error: {e}");
            }
            Err(_) => {
                log::warn!("SourceProxy::remove_position_handle('{query_id}') thread panicked");
            }
        }
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let deprovision_fn = self.vtable.deprovision_fn;
        let result = std::thread::spawn(move || (deprovision_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        let state_store_vtable = context
            .state_store
            .as_ref()
            .map(|ss| StateStoreVtableBuilder::build(ss.clone()));

        let wal_provider_vtable = context
            .wal_provider
            .as_ref()
            .map(|wp| crate::wal_provider_bridge::WalProviderVtableBuilder::build(wp.clone()));

        let instance_id_str = context.instance_id.clone();
        let component_id_str = context.source_id.clone();

        let instance_id_ffi = FfiStr::from_str(&instance_id_str);
        let component_id_ffi = FfiStr::from_str(&component_id_str);

        let ss_ptr = state_store_vtable
            .map(|v| Box::into_raw(Box::new(v)) as *const _)
            .unwrap_or(std::ptr::null());

        let wp_ptr = wal_provider_vtable
            .map(|v| Box::into_raw(Box::new(v)) as *const _)
            .unwrap_or(std::ptr::null());

        // Create per-instance callback context that routes logs to the
        // ComponentLogRegistry and lifecycle events through the SourceManager's
        // event channel (same path as static sources).
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
        // and intentionally leaks one strong ref per instance — acceptable
        // because the cdylib itself is intentionally process-leaked (see
        // host-sdk/src/loader.rs).
        let ctx_for_plugin = per_instance_ctx.clone();
        let ctx_ptr = Arc::into_raw(ctx_for_plugin) as *mut c_void;

        // Store the Arc so it stays alive as long as this proxy
        if let Ok(mut guard) = self._callback_ctx.lock() {
            *guard = Some(per_instance_ctx);
        }

        let identity_vtable = crate::proxies::identity_resolution::resolve_identity_provider(
            &self.identity_provider,
            context.identity_provider.clone(),
            &format!("Source '{}'", self.cached_id),
        )
        .map(crate::identity_bridge::IdentityProviderVtableBuilder::build);

        let ip_ptr: *mut drasi_plugin_sdk::ffi::identity::IdentityProviderVtable = identity_vtable
            .map(|v| Box::into_raw(Box::new(v)))
            .unwrap_or(std::ptr::null_mut());

        let ffi_ctx = FfiRuntimeContext {
            instance_id: instance_id_ffi,
            component_id: component_id_ffi,
            state_store: ss_ptr,
            identity_provider: ip_ptr as *const _,
            log_callback: Some(crate::callbacks::instance_log_callback),
            log_ctx: ctx_ptr,
            lifecycle_callback: Some(crate::callbacks::instance_lifecycle_callback),
            lifecycle_ctx: ctx_ptr,
            snapshot_fetcher: std::ptr::null(),
            wal_provider: wp_ptr,
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

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        // Wrap the host-side BootstrapProvider into a BootstrapProviderVtable
        // using the SDK's vtable generation.
        // The host executor runs futures on the current tokio runtime via std::thread::spawn.
        let vtable =
            drasi_plugin_sdk::ffi::build_bootstrap_provider_vtable(provider, host_executor);
        let vtable_ptr = Box::into_raw(Box::new(vtable));
        (self.vtable.set_bootstrap_provider_fn)(self.vtable.state, vtable_ptr);
    }

    /// Stash a per-instance identity provider that will take precedence over
    /// the runtime-context provider during [`Source::initialize`].
    ///
    /// # Timing constraint (FFI sources only)
    ///
    /// For `SourceProxy`, the provider must be set **before** the source is
    /// added to `DrasiLib` (i.e. before the lifecycle manager calls
    /// `initialize`). There is no FFI hook for late identity-provider
    /// injection — the plugin only receives the provider through
    /// `FfiRuntimeContext` during `initialize_fn`. Calls made after
    /// `initialize` have no effect on the running plugin.
    async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        // See doc comment above for the timing constraint.
        match self.identity_provider.lock() {
            Ok(mut guard) => *guard = Some(provider),
            Err(_) => log::warn!(
                "Source '{}': identity_provider mutex is poisoned; provider not set",
                self.cached_id
            ),
        }
    }
}

impl Drop for SourceProxy {
    fn drop(&mut self) {
        // Close the result channel to unblock the plugin forwarder (if any), then
        // wait for it to fully exit before freeing the wrapper. This mirrors the
        // ReactionProxy coordination: the plugin forwarder references the wrapper
        // through a raw pointer, so drop_fn must not run until the forwarder has
        // signalled (via the sentinel callback) that it will no longer touch it.
        {
            if let Ok(mut guard) = self.result_tx.lock() {
                *guard = None;
            }
        }

        let forwarder_exited = if let Ok(guard) = self._push_ctx.lock() {
            if let Some(ref ctx) = *guard {
                let done = ctx.forwarder_done.lock().expect("forwarder_done lock");
                let (done, timeout) = ctx
                    .forwarder_done_cv
                    .wait_timeout_while(done, std::time::Duration::from_secs(5), |done| !*done)
                    .expect("forwarder_done condvar wait");
                !timeout.timed_out() && *done
            } else {
                true // No push context → forwarder was never started
            }
        } else {
            false // Lock poisoned
        };

        if forwarder_exited {
            // Run plugin drop on the shared worker thread to avoid TLS destructor
            // races on macOS arm64 (see drop_worker module for details).
            let drop_fn = self.vtable.drop_fn;
            let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
            super::drop_worker::execute_drop_fn(drop_fn, state);
        } else {
            // Timeout or error — leak the source wrapper rather than risk a
            // use-after-free while the forwarder may still be running.
            log::warn!(
                "SourceProxy::drop: forwarder did not exit within timeout; \
                 leaking source wrapper to prevent use-after-free"
            );
        }

        // Leak the push context Arc so the forwarder's in-flight spawn_blocking
        // callback (on the timeout path) cannot dereference freed memory.
        if let Ok(mut guard) = self._push_ctx.lock() {
            if let Some(ctx) = guard.take() {
                std::mem::forget(ctx);
            }
        }

        // Bug C fix: leak the per-instance callback context Arc unconditionally.
        // The strong reference handed to the plugin via `Arc::into_raw` in
        // initialize() is never reclaimed — late log/lifecycle callbacks
        // emitted by the plugin (during stop_fn or from internal tasks) must
        // still find a valid pointer. Matches the pattern in ReactionProxy.
        if let Ok(mut guard) = self._callback_ctx.lock() {
            if let Some(ctx) = guard.take() {
                std::mem::forget(ctx);
            }
        }
    }
}

// ============================================================================
// SourcePluginProxy — wraps SourcePluginVtable into SourcePluginDescriptor
// ============================================================================

/// Wraps a `SourcePluginVtable` (factory) into a `SourcePluginDescriptor`.
///
/// The host uses this to create `SourceProxy` instances from configuration.
pub struct SourcePluginProxy {
    vtable: SourcePluginVtable,
    library: Arc<Library>,
    cached_kind: String,
    cached_config_version: String,
    cached_config_schema_name: String,
    plugin_id: String,
}

unsafe impl Send for SourcePluginProxy {}
unsafe impl Sync for SourcePluginProxy {}

impl SourcePluginProxy {
    pub fn new(vtable: SourcePluginVtable, library: Arc<Library>) -> Self {
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
impl SourcePluginDescriptor for SourcePluginProxy {
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

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Source>> {
        let config_str = serde_json::to_string(config_json)?;
        let id_ffi = FfiStr::from_str(id);
        let config_ffi = FfiStr::from_str(&config_str);

        let state = self.vtable.state;
        let create_fn = self.vtable.create_source_fn;
        let result = (create_fn)(state, id_ffi, config_ffi, auto_start);

        let vtable_ptr = unsafe {
            result
                .into_result::<SourceVtable>()
                .map_err(|msg| anyhow::anyhow!("{msg}"))?
        };

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for source '{id}'"
            ));
        }

        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(SourceProxy::new(vtable, self.library.clone())))
    }
}

impl Drop for SourcePluginProxy {
    fn drop(&mut self) {
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);
    }
}
