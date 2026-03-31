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

use drasi_lib::reactions::Reaction;
use drasi_lib::{ComponentStatus, ReactionRuntimeContext};
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;
use drasi_plugin_sdk::ffi::{
    FfiComponentStatus, FfiRuntimeContext, FfiStr, ReactionPluginVtable, ReactionVtable,
};
use libloading::Library;

use crate::state_store_bridge::StateStoreVtableBuilder;

/// Wraps a `ReactionVtable` into a DrasiLib `Reaction` trait implementation.
pub struct ReactionProxy {
    vtable: ReactionVtable,
    _library: Arc<Library>,
    cached_id: String,
    cached_type_name: String,
    _callback_ctx: std::sync::Mutex<Option<Arc<crate::callbacks::InstanceCallbackContext>>>,
    /// Channel for push-based result delivery. Created on start, closed on stop/drop.
    result_tx:
        std::sync::Mutex<Option<std::sync::mpsc::SyncSender<drasi_lib::channels::QueryResult>>>,
    /// Keep the callback context alive for the lifetime of the forwarder.
    _push_ctx: std::sync::Mutex<Option<Arc<ResultPushContext>>>,
}

/// Context for the push-based result callback.
struct ResultPushContext {
    rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<drasi_lib::channels::QueryResult>>>,
}

/// Callback invoked by the plugin's forwarder task to receive the next QueryResult.
/// Blocks until a result is available. Returns null on channel close (shutdown).
extern "C" fn result_push_callback(ctx: *mut c_void, _unused: *mut c_void) -> *mut c_void {
    let context = unsafe { &*(ctx as *const ResultPushContext) };
    let guard = context
        .rx
        .lock()
        .expect("result_push_callback lock poisoned");
    if let Some(ref rx) = *guard {
        match rx.recv() {
            Ok(result) => Box::into_raw(Box::new(result)) as *mut c_void,
            Err(_) => std::ptr::null_mut(),
        }
    } else {
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
        serde_json::from_str(&json_str).unwrap_or_default()
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

        let ctx_ptr = Arc::as_ptr(&per_instance_ctx) as *mut c_void;

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

    async fn start(&self) -> anyhow::Result<()> {
        // Set up push-based result channel before starting the reaction
        let (tx, rx) = std::sync::mpsc::sync_channel::<drasi_lib::channels::QueryResult>(256);
        {
            let mut guard = self.result_tx.lock().expect("result_tx lock poisoned");
            *guard = Some(tx);
        }

        let push_ctx = Arc::new(ResultPushContext {
            rx: std::sync::Mutex::new(Some(rx)),
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
        // Close the sender so the forwarder's callback returns null
        {
            let mut guard = self.result_tx.lock().expect("result_tx lock poisoned");
            *guard = None;
        }
        // Also drop the receiver to unblock the callback if it's blocked in recv()
        if let Ok(guard) = self._push_ctx.lock() {
            if let Some(ref ctx) = *guard {
                if let Ok(mut rx_guard) = ctx.rx.lock() {
                    *rx_guard = None;
                }
            }
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
        }
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        let guard = self.result_tx.lock().expect("result_tx lock poisoned");
        if let Some(ref tx) = *guard {
            tx.send(result)
                .map_err(|_| anyhow::anyhow!("Result channel closed"))?;
        } else {
            return Err(anyhow::anyhow!(
                "Reaction not started — result channel not initialized"
            ));
        }
        Ok(())
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let deprovision_fn = self.vtable.deprovision_fn;
        let result = std::thread::spawn(move || (deprovision_fn)(state.as_ptr()))
            .join()
            .map_err(|_| anyhow::anyhow!("Thread panicked"))?;
        unsafe { result.into_result().map_err(|e| anyhow::anyhow!(e)) }
    }
}

impl Drop for ReactionProxy {
    fn drop(&mut self) {
        // Close the result channel to unblock the forwarder
        if let Ok(mut guard) = self.result_tx.lock() {
            *guard = None;
        }
        // Also drop the receiver inside the push context to unblock the callback
        if let Ok(guard) = self._push_ctx.lock() {
            if let Some(ref ctx) = *guard {
                if let Ok(mut rx_guard) = ctx.rx.lock() {
                    *rx_guard = None;
                }
            }
        }
        let drop_fn = self.vtable.drop_fn;
        let state = drasi_plugin_sdk::ffi::SendMutPtr(self.vtable.state);
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();
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
        let _ = std::thread::spawn(move || (drop_fn)(state.as_ptr())).join();
    }
}
