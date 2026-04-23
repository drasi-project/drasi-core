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
use drasi_lib::sources::Source;
use drasi_lib::{ComponentStatus, DispatchMode, SourceRuntimeContext};
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;
use drasi_plugin_sdk::ffi::{
    FfiComponentStatus, FfiDispatchMode, FfiRuntimeContext, FfiStr, SourcePluginVtable,
    SourceVtable,
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

        let resp_ptr = (self.vtable.subscribe_fn)(
            self.vtable.state,
            source_id_ffi,
            enable_bootstrap,
            query_id_ffi,
            nodes_ffi,
            relations_ffi,
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

        Ok(SubscriptionResponse {
            query_id,
            source_id,
            receiver,
            bootstrap_receiver,
            position_handle: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
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

        let instance_id_str = context.instance_id.clone();
        let component_id_str = context.source_id.clone();

        let instance_id_ffi = FfiStr::from_str(&instance_id_str);
        let component_id_ffi = FfiStr::from_str(&component_id_str);

        let ss_ptr = state_store_vtable
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

        let identity_vtable = context
            .identity_provider
            .as_ref()
            .map(|ip| crate::identity_bridge::IdentityProviderVtableBuilder::build(ip.clone()));

        let ip_ptr = identity_vtable
            .map(|v| Box::into_raw(Box::new(v)) as *const _)
            .unwrap_or(std::ptr::null());

        let ffi_ctx = FfiRuntimeContext {
            instance_id: instance_id_ffi,
            component_id: component_id_ffi,
            state_store: ss_ptr,
            identity_provider: ip_ptr,
            log_callback: Some(crate::callbacks::instance_log_callback),
            log_ctx: ctx_ptr,
            lifecycle_callback: Some(crate::callbacks::instance_lifecycle_callback),
            lifecycle_ctx: ctx_ptr,
        };

        (self.vtable.initialize_fn)(self.vtable.state, &ffi_ctx as *const FfiRuntimeContext);
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
}

impl Drop for SourceProxy {
    fn drop(&mut self) {
        // Run plugin drop on a dedicated thread to avoid initializing
        // plugin TLS on the caller's thread (macOS TLS cleanup deadlock).
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();

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
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();
    }
}
