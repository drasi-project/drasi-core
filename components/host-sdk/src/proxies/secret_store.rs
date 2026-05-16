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

//! Host-side proxy for SecretStoreProvider and SecretStorePluginDescriptor.

use std::ffi::c_void;
use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::secret_store::SecretStoreProvider;
use drasi_plugin_sdk::descriptor::SecretStorePluginDescriptor;
use drasi_plugin_sdk::ffi::secret_store::SecretStoreProviderVtable;
use drasi_plugin_sdk::ffi::{FfiStr, SecretStorePluginVtable};
use libloading::Library;

// ============================================================================
// HostSecretStoreProxy — wraps SecretStoreProviderVtable → SecretStoreProvider
// ============================================================================

/// Wraps a `SecretStoreProviderVtable` returned by a plugin factory into the
/// `SecretStoreProvider` trait for use by the host.
pub struct HostSecretStoreProxy {
    vtable: SecretStoreProviderVtable,
    _library: Arc<Library>,
}

unsafe impl Send for HostSecretStoreProxy {}
unsafe impl Sync for HostSecretStoreProxy {}

impl HostSecretStoreProxy {
    pub fn new(vtable: SecretStoreProviderVtable, library: Arc<Library>) -> Self {
        Self {
            vtable,
            _library: library,
        }
    }
}

#[async_trait]
impl SecretStoreProvider for HostSecretStoreProxy {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        let state_addr = self.vtable.state as usize;
        let get_fn = self.vtable.get_secret_fn;
        let name_owned = name.to_string();

        // The get_secret_fn is blocking (dispatch_to_runtime inside plugin).
        // Call from a separate thread to avoid blocking the host's tokio runtime.
        let result = std::thread::spawn(move || {
            let state = state_addr as *const c_void;
            let name_ffi = FfiStr::from_str(&name_owned);
            let ffi_result = (get_fn)(state, name_ffi);
            unsafe { ffi_result.into_result() }
        })
        .join()
        .map_err(|_| anyhow::anyhow!("SecretStoreProvider thread panicked"))?;
        result
    }
}

impl Drop for HostSecretStoreProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}

// ============================================================================
// SecretStorePluginProxy — wraps SecretStorePluginVtable → descriptor
// ============================================================================

/// Wraps a `SecretStorePluginVtable` (factory) into a
/// `SecretStorePluginDescriptor` for the host to create provider instances.
pub struct SecretStorePluginProxy {
    vtable: SecretStorePluginVtable,
    library: Arc<Library>,
    cached_kind: String,
    cached_config_version: String,
    cached_config_schema_name: String,
}

unsafe impl Send for SecretStorePluginProxy {}
unsafe impl Sync for SecretStorePluginProxy {}

impl SecretStorePluginProxy {
    pub fn new(vtable: SecretStorePluginVtable, library: Arc<Library>) -> Self {
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
impl SecretStorePluginDescriptor for SecretStorePluginProxy {
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

    async fn create_secret_store(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn SecretStoreProvider>> {
        let config_str = serde_json::to_string(config_json)?;
        let config_ffi = FfiStr::from_str(&config_str);

        let state = self.vtable.state;
        let create_fn = self.vtable.create_secret_store_fn;
        let vtable_ptr = (create_fn)(state, config_ffi);

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for secret store '{}'",
                self.cached_kind
            ));
        }

        // Safety: vtable_ptr is non-null (checked above) and was allocated
        // by Box::into_raw in the plugin's create_secret_store_fn.
        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(HostSecretStoreProxy::new(
            vtable,
            self.library.clone(),
        )))
    }
}

impl Drop for SecretStorePluginProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}
