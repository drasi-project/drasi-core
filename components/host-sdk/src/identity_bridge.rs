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

//! Bridge from host-side `IdentityProvider` to FFI `IdentityProviderVtable`.
//!
//! The host creates an `IdentityProviderVtable` wrapping its real
//! `Arc<dyn IdentityProvider>` and passes it to plugins via `FfiRuntimeContext`.
//! Plugins use the vtable through `FfiIdentityProviderProxy` (in the plugin SDK)
//! to obtain credentials for external system authentication.

use std::ffi::c_void;
use std::sync::Arc;

use drasi_lib::identity::{CredentialContext, IdentityProvider};
use drasi_plugin_sdk::ffi::{credentials_to_ffi, FfiCredentialsResult, IdentityProviderVtable};

/// Builds an `IdentityProviderVtable` from a host-side `Arc<dyn IdentityProvider>`.
pub struct IdentityProviderVtableBuilder;

impl IdentityProviderVtableBuilder {
    /// Build a vtable wrapping the given identity provider.
    ///
    /// The returned vtable owns an Arc reference. The plugin can clone_fn to
    /// create additional references and must call drop_fn when done.
    pub fn build(provider: Arc<dyn IdentityProvider>) -> IdentityProviderVtable {
        extern "C" fn get_credentials_fn(
            state: *const c_void,
            context_json: *const u8,
            context_len: usize,
        ) -> FfiCredentialsResult {
            let provider = unsafe { &*(state as *const Arc<dyn IdentityProvider>) };
            let provider_clone = provider.clone();

            // Deserialize context from JSON
            let context = if context_json.is_null() || context_len == 0 {
                CredentialContext::default()
            } else {
                let json_bytes = unsafe { std::slice::from_raw_parts(context_json, context_len) };
                let json_str = std::str::from_utf8(json_bytes).unwrap_or("{}");
                let properties: std::collections::HashMap<String, String> =
                    serde_json::from_str(json_str).unwrap_or_default();
                CredentialContext { properties }
            };

            // Spawn a separate thread to avoid nesting tokio block_on calls.
            let result = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create runtime: {e}"))?;

                rt.block_on(provider_clone.get_credentials(&context))
            })
            .join();

            match result {
                Ok(Ok(creds)) => FfiCredentialsResult::ok(credentials_to_ffi(creds)),
                Ok(Err(e)) => FfiCredentialsResult::err(e.to_string()),
                Err(_) => FfiCredentialsResult::err("get_credentials thread panicked".to_string()),
            }
        }

        extern "C" fn clone_fn(state: *const c_void) -> *mut c_void {
            let provider = unsafe { &*(state as *const Arc<dyn IdentityProvider>) };
            let cloned = provider.clone();
            Box::into_raw(Box::new(cloned)) as *mut c_void
        }

        extern "C" fn drop_fn(state: *mut c_void) {
            if !state.is_null() {
                unsafe { drop(Box::from_raw(state as *mut Arc<dyn IdentityProvider>)) };
            }
        }

        let state = Box::into_raw(Box::new(provider)) as *mut c_void;

        IdentityProviderVtable {
            state,
            get_credentials_fn,
            clone_fn,
            drop_fn,
        }
    }
}
