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
}

unsafe impl Send for ReactionProxy {}
unsafe impl Sync for ReactionProxy {}

impl ReactionProxy {
    pub fn new(vtable: ReactionVtable, library: Arc<Library>) -> Self {
        let cached_id =
            unsafe { (vtable.id_fn)(vtable.state as *const c_void).to_string() };
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
impl Reaction for ReactionProxy {
    fn id(&self) -> &str {
        &self.cached_id
    }

    fn type_name(&self) -> &str {
        &self.cached_type_name
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        let arr = (self.vtable.query_ids_fn)(self.vtable.state as *const c_void);
        let ids = unsafe { arr.into_vec() };
        ids
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
            log_registry: drasi_lib::managers::get_or_init_global_registry(),
            event_tx: context.status_tx.clone(),
        });

        let ctx_ptr = crate::callbacks::InstanceCallbackContext::into_raw(per_instance_ctx.clone());

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
        (self.vtable.drop_fn)(self.vtable.state);
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
}

unsafe impl Send for ReactionPluginProxy {}
unsafe impl Sync for ReactionPluginProxy {}

impl ReactionPluginProxy {
    pub fn new(vtable: ReactionPluginVtable, library: Arc<Library>) -> Self {
        let cached_kind =
            unsafe { (vtable.kind_fn)(vtable.state as *const c_void).to_string() };
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
        let vtable_ptr = (create_fn)(state, id_ffi, query_ids_ffi, config_ffi, auto_start);

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for reaction '{}'",
                id
            ));
        }

        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(ReactionProxy::new(vtable, self.library.clone())))
    }
}

impl Drop for ReactionPluginProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}
