// Copyright 2026 The Drasi Authors.
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

//! Host-side proxy for IdentityProvider and IdentityProviderPluginDescriptor.

use std::ffi::c_void;
use std::sync::Arc;

use async_trait::async_trait;

use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use drasi_plugin_sdk::descriptor::IdentityProviderPluginDescriptor;
use drasi_plugin_sdk::ffi::{FfiStr, IdentityProviderPluginVtable};
use libloading::Library;

// ============================================================================
// HostIdentityProviderProxy — wraps IdentityProviderVtable → IdentityProvider
// ============================================================================

/// Wraps an `IdentityProviderVtable` returned by a plugin factory into the
/// `IdentityProvider` trait for use by the host (e.g., injecting into
/// source/reaction runtime contexts).
pub struct HostIdentityProviderProxy {
    vtable: drasi_plugin_sdk::ffi::identity::IdentityProviderVtable,
    _library: Arc<Library>,
}

unsafe impl Send for HostIdentityProviderProxy {}
unsafe impl Sync for HostIdentityProviderProxy {}

impl HostIdentityProviderProxy {
    pub fn new(
        vtable: drasi_plugin_sdk::ffi::identity::IdentityProviderVtable,
        library: Arc<Library>,
    ) -> Self {
        Self {
            vtable,
            _library: library,
        }
    }
}

#[async_trait]
impl IdentityProvider for HostIdentityProviderProxy {
    async fn get_credentials(&self, context: &CredentialContext) -> anyhow::Result<Credentials> {
        let state_addr = self.vtable.state as usize;
        let get_fn = self.vtable.get_credentials_fn;
        // Serialize context as JSON for FFI transport
        let context_json =
            serde_json::to_string(&context.properties).unwrap_or_else(|_| "{}".to_string());

        // The get_credentials_fn is blocking — call from a separate thread.
        // We pass state as usize (which is Send) and reconstruct the pointer inside.
        // Safety: The state pointer is backed by an Arc<dyn IdentityProvider> and is
        // safe to use from another thread (the vtable is Send+Sync).
        let result = std::thread::spawn(move || {
            let state = state_addr as *const c_void;
            let ffi_result = (get_fn)(state, context_json.as_ptr(), context_json.len());
            unsafe { ffi_result.into_result() }
        })
        .join()
        .map_err(|_| anyhow::anyhow!("IdentityProvider thread panicked"))?;
        result
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        assert!(
            !self.vtable.state.is_null(),
            "IdentityProvider state is null during clone"
        );
        let cloned_state = (self.vtable.clone_fn)(self.vtable.state as *const c_void);
        let cloned_vtable = drasi_plugin_sdk::ffi::identity::IdentityProviderVtable {
            state: cloned_state,
            get_credentials_fn: self.vtable.get_credentials_fn,
            clone_fn: self.vtable.clone_fn,
            drop_fn: self.vtable.drop_fn,
        };
        Box::new(HostIdentityProviderProxy {
            vtable: cloned_vtable,
            _library: self._library.clone(),
        })
    }
}

impl Drop for HostIdentityProviderProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}

// ============================================================================
// IdentityProviderPluginProxy — wraps IdentityProviderPluginVtable → descriptor
// ============================================================================

/// Wraps an `IdentityProviderPluginVtable` (factory) into an
/// `IdentityProviderPluginDescriptor` for the host to create provider instances.
pub struct IdentityProviderPluginProxy {
    vtable: IdentityProviderPluginVtable,
    library: Arc<Library>,
    cached_kind: String,
    cached_config_version: String,
    cached_config_schema_name: String,
}

unsafe impl Send for IdentityProviderPluginProxy {}
unsafe impl Sync for IdentityProviderPluginProxy {}

impl IdentityProviderPluginProxy {
    pub fn new(vtable: IdentityProviderPluginVtable, library: Arc<Library>) -> Self {
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
impl IdentityProviderPluginDescriptor for IdentityProviderPluginProxy {
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

    async fn create_identity_provider(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn IdentityProvider>> {
        let config_str = serde_json::to_string(config_json)?;
        let config_ffi = FfiStr::from_str(&config_str);

        let state = self.vtable.state;
        let create_fn = self.vtable.create_identity_provider_fn;
        let vtable_ptr = (create_fn)(state, config_ffi);

        if vtable_ptr.is_null() {
            return Err(anyhow::anyhow!(
                "Plugin factory returned null for identity provider '{}'",
                self.cached_kind
            ));
        }

        // Safety: vtable_ptr is non-null (checked above) and was allocated
        // by Box::into_raw in the plugin's create_identity_provider_fn.
        let vtable = unsafe { *Box::from_raw(vtable_ptr) };
        Ok(Box::new(HostIdentityProviderProxy::new(
            vtable,
            self.library.clone(),
        )))
    }
}

impl Drop for IdentityProviderPluginProxy {
    fn drop(&mut self) {
        (self.vtable.drop_fn)(self.vtable.state);
    }
}
