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
    // Wrap the raw pointer to make it Send-safe for std::thread::spawn
    let send_ptr = drasi_plugin_sdk::ffi::SendMutPtr(future_ptr);
    let result = std::thread::spawn(move || {
        let boxed_future = unsafe {
            Box::from_raw(send_ptr.as_ptr()
                as *mut std::pin::Pin<Box<dyn std::future::Future<Output = *mut c_void>>>)
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Wrap the result in SendMutPtr to satisfy Send bound
        drasi_plugin_sdk::ffi::SendMutPtr(rt.block_on(*boxed_future))
    })
    .join()
    .expect("host executor thread panicked");
    result.as_ptr()
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
        // Dynamic plugins don't expose properties through FFI yet
        HashMap::new()
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

        let state = self.vtable.state;
        let subscribe_fn = self.vtable.subscribe_fn;

        let resp_ptr = (subscribe_fn)(
            state,
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
            Box::new(ChangeReceiverProxy::new(ffi_cr))
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
            Some(BootstrapReceiverProxy::new(ffi_br).into_mpsc_receiver())
        };

        Ok(SubscriptionResponse {
            query_id,
            source_id,
            receiver,
            bootstrap_receiver,
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
            log_registry: drasi_lib::managers::get_or_init_global_registry(),
            event_tx: context.status_tx.clone(),
        });

        let ctx_ptr = crate::callbacks::InstanceCallbackContext::into_raw(per_instance_ctx.clone());

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
        (self.vtable.drop_fn)(self.vtable.state);
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
        }
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
        let vtable_ptr = (create_fn)(state, id_ffi, config_ffi, auto_start);

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for source '{}'",
                id
            ));
        }

        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(SourceProxy::new(vtable, self.library.clone())))
    }
}

impl Drop for SourcePluginProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}
