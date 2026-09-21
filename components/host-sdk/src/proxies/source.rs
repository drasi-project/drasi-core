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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use anyhow::Context;
use async_trait::async_trait;

use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::events::SubscriptionResponse;
use drasi_lib::channels::BootstrapEventSender;
use drasi_lib::component_graph::ComponentStatusHandle;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::{ComponentResource, ComponentResourceObserver, PluginOrigin};
use drasi_lib::identity::IdentityProvider;
use drasi_lib::schema::SourceSchema;
use drasi_lib::sources::Source;
use drasi_lib::{ComponentStatus, DispatchMode, SourceRuntimeContext};
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;
use drasi_plugin_sdk::ffi::{
    FfiComponentStatus, FfiDispatchMode, FfiRuntimeContext, FfiStr, PluginMetadata,
    SourcePluginVtable, SourceVtable,
};
use libloading::Library;

use super::change_receiver::{BootstrapReceiverProxy, ChangeReceiverProxy};
use crate::state_store_bridge::StateStoreVtableBuilder;

pub(crate) fn read_plugin_version(library: &Library) -> Option<String> {
    // Read the existing export from the retained library, never reopening its
    // path or substituting the descriptor's unrelated configuration version.
    let metadata_fn = unsafe {
        library.get::<unsafe extern "C" fn() -> *const PluginMetadata>(b"drasi_plugin_metadata")
    };
    let metadata_fn = match metadata_fn {
        Ok(metadata_fn) => metadata_fn,
        Err(error) => {
            log::debug!("Plugin origin metadata is unavailable: {error}");
            return None;
        }
    };
    unsafe { copy_plugin_version(metadata_fn()) }
}

// The metadata and its borrowed FfiStr buffers must remain valid for this call.
unsafe fn copy_plugin_version(metadata: *const PluginMetadata) -> Option<String> {
    let metadata = unsafe { metadata.as_ref() }?;
    let version = unsafe { metadata.plugin_version.to_string() };
    (!version.is_empty()).then_some(version)
}

pub(super) fn known_plugin_origin(id: &str, version: Option<&str>) -> Option<PluginOrigin> {
    let version = version.filter(|version| !version.is_empty())?;
    (!id.is_empty()).then(|| PluginOrigin {
        id: id.to_owned(),
        version: version.to_owned(),
    })
}

pub(super) async fn observe_resources_and_plugin(
    observer: &dyn ComponentResourceObserver,
    resources: Vec<ComponentResource>,
    origin: Option<&PluginOrigin>,
) -> anyhow::Result<()> {
    let plugin_result = match origin {
        Some(origin) => observer
            .observe_plugin(origin.clone())
            .await
            .context("Plugin origin observation failed"),
        None => Ok(()),
    };
    let resource_result = observer
        .observe(resources)
        .await
        .context("Provider inventory observation failed");

    match (plugin_result, resource_result) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(plugin_error), Err(resource_error)) => {
            Err(resource_error.context(format!("{plugin_error:#}")))
        }
    }
}

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
    /// Per-source identity provider set programmatically via
    /// [`Source::set_identity_provider`]. When present, it takes precedence over
    /// any instance-wide provider supplied via
    /// [`SourceRuntimeContext::identity_provider`] during [`Source::initialize`].
    identity_provider: std::sync::Mutex<Option<Arc<dyn IdentityProvider>>>,
    resources: tokio::sync::Mutex<SourceResourceInventory>,
    resource_report_failed: AtomicBool,
    plugin_origin: Option<PluginOrigin>,
}

#[derive(Default)]
struct SourceResourceInventory {
    // The FFI forwarding wrapper owns the strong reference, so legacy mode does
    // not prolong provider lifetime merely to support a future observation.
    bootstrap_provider: Option<Weak<dyn BootstrapProvider>>,
    // Retained only when observation is enabled, using the identity actually
    // passed to the plugin rather than a later, unsupported identity setter.
    context: Option<SourceRuntimeContext>,
}

impl SourceResourceInventory {
    fn set_context(
        &mut self,
        context: &SourceRuntimeContext,
        identity_provider: Option<Arc<dyn IdentityProvider>>,
    ) {
        self.context = context.resource_observer.as_ref().map(|_| {
            let mut selected_context = context.clone();
            selected_context.identity_provider = identity_provider;
            selected_context
        });
    }

    fn share_bootstrap(
        &mut self,
        provider: Box<dyn BootstrapProvider>,
    ) -> Box<dyn BootstrapProvider> {
        let provider: Arc<dyn BootstrapProvider> = Arc::from(provider);
        self.bootstrap_provider = Some(Arc::downgrade(&provider));
        Box::new(SharedBootstrapProvider(provider))
    }

    async fn report(&self, failed: &AtomicBool, origin: Option<&PluginOrigin>) {
        let Some(context) = self.context.as_ref() else {
            return;
        };
        let Some(observer) = context.resource_observer.as_ref() else {
            return;
        };

        let mut resources = Vec::new();
        if let Some(provider) = self.bootstrap_provider.as_ref().and_then(Weak::upgrade) {
            resources.push(ComponentResource::Bootstrap(provider));
        }
        if let Some(provider) = context.identity_provider.as_ref() {
            resources.push(ComponentResource::Identity(provider.clone()));
        }
        if let Some(provider) = context.state_store.as_ref() {
            resources.push(ComponentResource::StateStore(provider.clone()));
        }
        if let Some(provider) = context.wal_provider.as_ref() {
            resources.push(ComponentResource::Wal(provider.clone()));
        }

        match observe_resources_and_plugin(observer.as_ref(), resources, origin).await {
            Ok(()) => failed.store(false, Ordering::Release),
            Err(error) => {
                failed.store(true, Ordering::Release);
                let message = format!(
                    "Source '{}' failed to report component resources: {error:#}",
                    context.source_id
                );
                log::error!("{message}");
                ComponentStatusHandle::new_wired(&context.source_id, context.update_tx.clone())
                    .set_status(ComponentStatus::Error, Some(message))
                    .await;
            }
        }
    }
}

struct SharedBootstrapProvider(Arc<dyn BootstrapProvider>);

#[async_trait]
impl BootstrapProvider for SharedBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        settings: Option<&SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        self.0.bootstrap(request, context, event_tx, settings).await
    }
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
            identity_provider: std::sync::Mutex::new(None),
            resources: tokio::sync::Mutex::new(SourceResourceInventory::default()),
            resource_report_failed: AtomicBool::new(false),
            plugin_origin: None,
        }
    }

    fn with_plugin_origin(mut self, origin: Option<PluginOrigin>) -> Self {
        self.plugin_origin = origin;
        self
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
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let start_fn = self.vtable.start_fn;
        let result = std::thread::spawn(move || (start_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let stop_fn = self.vtable.stop_fn;
        let result = std::thread::spawn(move || (stop_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }

    async fn status(&self) -> ComponentStatus {
        if self.resource_report_failed.load(Ordering::Acquire) {
            return ComponentStatus::Error;
        }
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

        // Pass resume_sequence across FFI. 0 is the sentinel for None; real
        // sequences start at 1 (the framework counter starts at 1), so 0 never
        // collides with a genuine checkpoint — and a floor derived from 0 would
        // be 1 (the default) anyway, making the sentinel a no-op either way.
        // Lets out-of-process sources raise their sequence counter for restart
        // monotonicity.
        let resume_sequence = settings.resume_sequence.unwrap_or(0);

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
            resume_sequence,
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
                    // Extract (and free) the provider error text unconditionally
                    // so the buffer never leaks, whichever branch we take below.
                    let error_text = if !ffi_result.error_ptr.is_null() && ffi_result.error_len > 0
                    {
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
                        let _ = tx.send(Err(match error_text {
                            Some(msg) => anyhow::anyhow!("Bootstrap failed: {msg}"),
                            None => anyhow::anyhow!(
                                "Bootstrap failed with code {}",
                                ffi_result.event_count
                            ),
                        }));
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
        let identity_provider = crate::proxies::identity_resolution::resolve_identity_provider(
            &self.identity_provider,
            context.identity_provider.clone(),
            &format!("Source '{}'", self.cached_id),
        );
        let mut resources = self.resources.lock().await;
        resources.set_context(&context, identity_provider.clone());
        self.resource_report_failed.store(false, Ordering::Release);
        resources
            .report(&self.resource_report_failed, self.plugin_origin.as_ref())
            .await;

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

        let identity_vtable =
            identity_provider.map(crate::identity_bridge::IdentityProviderVtableBuilder::build);

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
        let mut resources = self.resources.lock().await;
        let provider = resources.share_bootstrap(provider);
        // Wrap the host-side BootstrapProvider into a BootstrapProviderVtable
        // using the SDK's vtable generation.
        // The host executor runs futures on the current tokio runtime via std::thread::spawn.
        {
            let vtable =
                drasi_plugin_sdk::ffi::build_bootstrap_provider_vtable(provider, host_executor);
            let vtable_ptr = Box::into_raw(Box::new(vtable));
            (self.vtable.set_bootstrap_provider_fn)(self.vtable.state, vtable_ptr);
        }
        resources
            .report(&self.resource_report_failed, self.plugin_origin.as_ref())
            .await;
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
        // Run plugin drop on the shared worker thread to avoid TLS destructor
        // races on macOS arm64 (see drop_worker module for details).
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);

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
    plugin_version: Option<String>,
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
        let plugin_version = read_plugin_version(&library);
        Self {
            vtable,
            library,
            cached_kind,
            cached_config_version,
            cached_config_schema_name,
            plugin_id: String::new(),
            plugin_version,
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
        Ok(Box::new(
            SourceProxy::new(vtable, self.library.clone()).with_plugin_origin(known_plugin_origin(
                &self.plugin_id,
                self.plugin_version.as_deref(),
            )),
        ))
    }
}

impl Drop for SourcePluginProxy {
    fn drop(&mut self) {
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        super::drop_worker::execute_drop_fn(drop_fn, state);
    }
}

#[cfg(test)]
pub(super) mod resource_observer_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::{mpsc, Mutex};

    use drasi_lib::component_graph::ComponentUpdate;
    use drasi_lib::context::ComponentResourceObserver;
    use drasi_lib::identity::PasswordIdentityProvider;
    use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreProvider};

    #[derive(Default)]
    struct RecordingObserver {
        reports: Mutex<Vec<Vec<ComponentResource>>>,
        origins: Mutex<Vec<PluginOrigin>>,
        fail: AtomicBool,
        fail_plugin: AtomicBool,
    }

    #[async_trait]
    impl ComponentResourceObserver for RecordingObserver {
        async fn observe(&self, resources: Vec<ComponentResource>) -> anyhow::Result<()> {
            self.reports.lock().await.push(resources);
            anyhow::ensure!(!self.fail.load(Ordering::Acquire), "inventory rejected");
            Ok(())
        }

        async fn observe_plugin(&self, origin: PluginOrigin) -> anyhow::Result<()> {
            self.origins.lock().await.push(origin);
            anyhow::ensure!(!self.fail_plugin.load(Ordering::Acquire), "origin rejected");
            Ok(())
        }
    }

    struct CountingBootstrap {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl BootstrapProvider for CountingBootstrap {
        async fn bootstrap(
            &self,
            request: BootstrapRequest,
            context: &BootstrapContext,
            _event_tx: BootstrapEventSender,
            settings: Option<&SourceSubscriptionSettings>,
        ) -> anyhow::Result<BootstrapResult> {
            assert_eq!(request.query_id, "query");
            assert_eq!(context.source_id, "source");
            assert_eq!(settings.unwrap().source_id, "source");
            Ok(BootstrapResult {
                event_count: self.calls.fetch_add(1, Ordering::AcqRel) + 1,
                source_position: None,
            })
        }
    }

    impl Drop for CountingBootstrap {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[tokio::test]
    async fn shared_bootstrap_forwards_to_the_reported_instance_and_drops_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut inventory = SourceResourceInventory::default();
        let forwarding = inventory.share_bootstrap(Box::new(CountingBootstrap {
            calls: calls.clone(),
            drops: drops.clone(),
        }));
        let weak = inventory.bootstrap_provider.as_ref().unwrap().clone();
        assert_eq!(weak.strong_count(), 1);

        let observer = Arc::new(RecordingObserver::default());
        let (update_tx, _rx) = mpsc::channel(8);
        let mut context = SourceRuntimeContext::new("instance", "source", None, update_tx, None);
        context.resource_observer = Some(observer.clone());
        inventory.set_context(&context, None);
        inventory.report(&AtomicBool::new(false), None).await;

        {
            let reports = observer.reports.lock().await;
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].len(), 1);
            let ComponentResource::Bootstrap(reported) = &reports[0][0] else {
                panic!("expected bootstrap");
            };
            assert!(Arc::ptr_eq(reported, &weak.upgrade().unwrap()));
            let settings = SourceSubscriptionSettings {
                source_id: "source".into(),
                query_id: "query".into(),
                enable_bootstrap: true,
                nodes: Default::default(),
                relations: Default::default(),
                resume_from: None,
                resume_sequence: None,
                request_position_handle: false,
            };
            for (provider, count) in [(forwarding.as_ref(), 1), (reported.as_ref(), 2)] {
                let (tx, _rx) = mpsc::channel(1);
                let result = provider
                    .bootstrap(
                        BootstrapRequest {
                            query_id: "query".into(),
                            node_labels: vec![],
                            relation_labels: vec![],
                            request_id: "request".into(),
                        },
                        &BootstrapContext::new_minimal("instance".into(), "source".into()),
                        tx,
                        Some(&settings),
                    )
                    .await
                    .unwrap();
                assert_eq!(result.event_count, count);
            }
        }
        assert_eq!(calls.load(Ordering::Acquire), 2);
        drop(forwarding);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        observer.reports.lock().await.clear();
        assert!(weak.upgrade().is_none());
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn legacy_inventory_retains_no_context_or_strong_bootstrap_reference() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let (update_tx, mut rx) = mpsc::channel(8);
        let context =
            SourceRuntimeContext::new("instance", "source", Some(store.clone()), update_tx, None);
        let mut inventory = SourceResourceInventory::default();
        inventory.set_context(&context, None);
        assert!(inventory.context.is_none());
        assert_eq!(Arc::strong_count(&store), 2);

        let drops = Arc::new(AtomicUsize::new(0));
        let forwarding = inventory.share_bootstrap(Box::new(CountingBootstrap {
            calls: Arc::new(AtomicUsize::new(0)),
            drops: drops.clone(),
        }));
        let failed = AtomicBool::new(false);
        inventory.report(&failed, None).await;
        assert!(!failed.load(Ordering::Acquire));
        drop(forwarding);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(inventory
            .bootstrap_provider
            .as_ref()
            .unwrap()
            .upgrade()
            .is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reports_effective_identity_not_context_default_or_unsupported_late_setter() {
        let selected: Arc<dyn IdentityProvider> =
            Arc::new(PasswordIdentityProvider::new("selected", "test"));
        let per_instance = std::sync::Mutex::new(Some(selected.clone()));
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let (update_tx, _rx) = mpsc::channel(8);
        let mut context = SourceRuntimeContext::new(
            "instance",
            "source",
            Some(store.clone()),
            update_tx,
            Some(Arc::new(PasswordIdentityProvider::new("default", "test"))),
        );
        let observer = Arc::new(RecordingObserver::default());
        context.resource_observer = Some(observer.clone());
        let resolved = crate::proxies::identity_resolution::resolve_identity_provider(
            &per_instance,
            context.identity_provider.clone(),
            "Source 'source'",
        );
        let mut inventory = SourceResourceInventory::default();
        inventory.set_context(&context, resolved);
        let failed = AtomicBool::new(false);
        inventory.report(&failed, None).await;

        *per_instance.lock().unwrap() =
            Some(Arc::new(PasswordIdentityProvider::new("late", "test")));
        let forwarding = inventory.share_bootstrap(Box::new(CountingBootstrap {
            calls: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
        }));
        inventory.report(&failed, None).await;
        let reports = observer.reports.lock().await;
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].len(), 2);
        assert_eq!(reports[1].len(), 3);
        for resources in [&reports[0][..], &reports[1][1..]] {
            let [ComponentResource::Identity(identity), ComponentResource::StateStore(state)] =
                resources
            else {
                panic!("expected selected identity and state store");
            };
            assert!(Arc::ptr_eq(identity, &selected));
            assert!(Arc::ptr_eq(state, &store));
        }
        drop(forwarding);
    }

    #[tokio::test]
    async fn failed_reports_signal_error_and_successful_refresh_clears_local_failure() {
        let observer = Arc::new(RecordingObserver::default());
        observer.fail.store(true, Ordering::Release);
        let (update_tx, mut rx) = mpsc::channel(8);
        let mut context = SourceRuntimeContext::new("instance", "source", None, update_tx, None);
        context.resource_observer = Some(observer.clone());
        let mut inventory = SourceResourceInventory::default();
        inventory.set_context(&context, None);
        let failed = AtomicBool::new(false);
        inventory.report(&failed, None).await;
        assert!(failed.load(Ordering::Acquire));
        let ComponentUpdate::Status {
            component_id,
            status,
            message,
        } = rx.try_recv().unwrap();
        assert_eq!(component_id, "source");
        assert_eq!(status, ComponentStatus::Error);
        assert!(message.unwrap().contains("inventory rejected"));

        observer.fail.store(false, Ordering::Release);
        inventory.report(&failed, None).await;
        assert!(!failed.load(Ordering::Acquire));
        assert_eq!(observer.reports.lock().await.len(), 2);
        assert!(rx.try_recv().is_err());
    }

    pub(crate) fn test_library() -> Arc<Library> {
        // Retain the existing process image; no plugin is loaded for these tests.
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this().into();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap().into();
        Arc::new(library)
    }

    pub(crate) fn test_runtime() -> &'static tokio::runtime::Runtime {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        })
    }

    pub(crate) extern "C" fn test_executor(future_ptr: *mut c_void) -> *mut c_void {
        host_executor(future_ptr)
    }

    pub(crate) fn metadata_version(version: Option<&str>) -> Option<String> {
        let Some(version) = version else {
            return unsafe { copy_plugin_version(std::ptr::null()) };
        };
        let metadata = PluginMetadata {
            sdk_version: FfiStr::from_str("0.14.0"),
            core_version: FfiStr::from_str("8.0.0"),
            lib_version: FfiStr::from_str("9.0.0"),
            plugin_version: FfiStr::from_str(version),
            target_triple: FfiStr::from_str("test-target"),
            git_commit: FfiStr::from_str("test-commit"),
            build_timestamp: FfiStr::from_str("test-timestamp"),
        };
        unsafe { copy_plugin_version(&metadata) }
    }

    struct OriginSource(String);

    #[async_trait]
    impl Source for OriginSource {
        fn id(&self) -> &str {
            &self.0
        }
        fn type_name(&self) -> &str {
            "unrelated-source-type"
        }
        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }
        async fn initialize(&self, context: SourceRuntimeContext) {
            assert!(context.resource_observer.is_none());
        }
        async fn start(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Stopped
        }
        async fn subscribe(
            &self,
            _settings: SourceSubscriptionSettings,
        ) -> anyhow::Result<SubscriptionResponse> {
            anyhow::bail!("not used by origin tests")
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct OriginSourceDescriptor;

    #[async_trait]
    impl SourcePluginDescriptor for OriginSourceDescriptor {
        fn kind(&self) -> &str {
            "unrelated-source-kind"
        }
        fn config_version(&self) -> &str {
            "99.0.0"
        }
        fn config_schema_json(&self) -> String {
            "{}".into()
        }
        fn config_schema_name(&self) -> &str {
            "OriginSourceConfig"
        }
        async fn create_source(
            &self,
            id: &str,
            _config: &serde_json::Value,
            _auto_start: bool,
        ) -> anyhow::Result<Box<dyn Source>> {
            Ok(Box::new(OriginSource(id.into())))
        }
    }

    fn origin_factory(id: &str, version: Option<&str>) -> SourcePluginProxy {
        let vtable = drasi_plugin_sdk::ffi::build_source_plugin_vtable(
            OriginSourceDescriptor,
            test_executor,
            |_, _, _| {},
            test_runtime,
        );
        let mut factory = SourcePluginProxy::new(vtable, test_library());
        factory.plugin_version = metadata_version(version);
        factory.set_plugin_id(id.into());
        factory
    }

    #[test]
    fn metadata_version_is_owned_and_never_uses_sdk_core_or_lib_versions() {
        let copied = {
            let version = String::from("1.2.3+build");
            metadata_version(Some(&version)).unwrap()
        };
        assert_eq!(copied, "1.2.3+build");
        assert!(metadata_version(None).is_none());
        assert!(metadata_version(Some("")).is_none());
    }

    #[tokio::test]
    async fn factory_propagates_only_explicit_id_and_metadata_version() {
        for (id, version) in [
            ("explicit-source-plugin", Some("1.2.3")),
            ("", Some("1.2.3")),
            ("explicit-source-plugin", None),
            ("explicit-source-plugin", Some("")),
        ] {
            for observed in [false, true] {
                let factory = origin_factory(id, version);
                assert_eq!(factory.config_version(), "99.0.0");
                let source = factory
                    .create_source("source", &serde_json::json!({}), false)
                    .await
                    .unwrap();
                let expected = if !id.is_empty() && version == Some("1.2.3") {
                    Some(PluginOrigin {
                        id: id.into(),
                        version: "1.2.3".into(),
                    })
                } else {
                    None
                };
                let proxy = source.as_any().downcast_ref::<SourceProxy>().unwrap();
                assert_eq!(proxy.plugin_origin, expected);
                let observer = Arc::new(RecordingObserver::default());
                let (update_tx, mut rx) = mpsc::channel(8);
                let mut context =
                    SourceRuntimeContext::new("instance", "source", None, update_tx, None);
                if observed {
                    context.resource_observer = Some(observer.clone());
                }
                source.initialize(context).await;
                let expected_origins: Vec<_> = expected.filter(|_| observed).into_iter().collect();
                assert_eq!(*observer.origins.lock().await, expected_origins);
                assert_eq!(observer.reports.lock().await.len(), usize::from(observed));
                assert_eq!(source.status().await, ComponentStatus::Stopped);
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn directly_constructed_source_has_no_inferred_origin() {
        let vtable = drasi_plugin_sdk::ffi::build_source_vtable(
            OriginSource("source".into()),
            test_executor,
            |_, _, _| {},
            test_runtime,
        );
        let source = SourceProxy::new(vtable, test_library());
        assert!(source.plugin_origin.is_none());
        let observer = Arc::new(RecordingObserver::default());
        let (update_tx, _rx) = mpsc::channel(8);
        let mut context = SourceRuntimeContext::new("instance", "source", None, update_tx, None);
        context.resource_observer = Some(observer.clone());
        source.initialize(context).await;
        assert!(observer.origins.lock().await.is_empty());
        assert_eq!(observer.reports.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn origin_failure_preserves_provider_reporting_and_error_status() {
        for reject_inventory in [false, true] {
            let source = origin_factory("explicit-source-plugin", Some("1.2.3"))
                .create_source("source", &serde_json::json!({}), false)
                .await
                .unwrap();
            let observer = Arc::new(RecordingObserver::default());
            observer.fail_plugin.store(true, Ordering::Release);
            observer.fail.store(reject_inventory, Ordering::Release);
            let (update_tx, mut rx) = mpsc::channel(8);
            let mut context =
                SourceRuntimeContext::new("instance", "source", None, update_tx, None);
            context.resource_observer = Some(observer.clone());
            source.initialize(context).await;
            assert_eq!(observer.origins.lock().await.len(), 1);
            assert_eq!(observer.reports.lock().await.len(), 1);
            assert_eq!(source.status().await, ComponentStatus::Error);
            let ComponentUpdate::Status {
                status, message, ..
            } = rx.try_recv().unwrap();
            assert_eq!(status, ComponentStatus::Error);
            let message = message.unwrap();
            assert!(message.contains("origin rejected"));
            assert_eq!(message.contains("inventory rejected"), reject_inventory);
        }
    }
}
