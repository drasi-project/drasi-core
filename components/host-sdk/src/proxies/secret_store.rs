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
use drasi_plugin_sdk::resolver::{ResolverError, ValueResolver};
use drasi_plugin_sdk::ConfigValue;
use libloading::Library;

/// Adapter that wraps a [`SecretStoreProvider`] into a [`ValueResolver`].
///
/// This bridges the `drasi-lib` secret store trait into the plugin-sdk's
/// resolver pipeline.
pub struct SecretStoreValueResolverAdapter {
    provider: Arc<dyn SecretStoreProvider>,
}

impl SecretStoreValueResolverAdapter {
    /// Create a new adapter wrapping the given secret store provider.
    pub fn new(provider: Arc<dyn SecretStoreProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl ValueResolver for SecretStoreValueResolverAdapter {
    async fn resolve_to_string(
        &self,
        value: &ConfigValue<String>,
    ) -> Result<String, ResolverError> {
        match value {
            ConfigValue::Secret { name } => self.provider.get_secret(name).await.map_err(|e| {
                ResolverError::SecretResolutionFailed(format!(
                    "Failed to resolve secret '{name}': {e}"
                ))
            }),
            _ => Err(ResolverError::WrongResolverType),
        }
    }
}

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
        // Use spawn_blocking to avoid blocking the host's tokio runtime.
        let result = tokio::task::spawn_blocking(move || {
            let state = state_addr as *const c_void;
            let name_ffi = FfiStr::from_str(&name_owned);
            let ffi_result = (get_fn)(state, name_ffi);
            unsafe { ffi_result.into_result() }
        })
        .await
        .map_err(|e| anyhow::anyhow!("SecretStoreProvider task panicked: {e}"))?;
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

#[cfg(test)]
mod tests {
    use super::SecretStoreValueResolverAdapter;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use drasi_lib::secret_store::SecretStoreProvider;
    use drasi_plugin_sdk::resolver::{ResolverError, ValueResolver};
    use drasi_plugin_sdk::ConfigValue;
    use std::collections::HashMap;
    use std::sync::Arc;

    struct MockSecretStore {
        secrets: HashMap<String, String>,
    }

    #[async_trait]
    impl SecretStoreProvider for MockSecretStore {
        async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
            self.secrets
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("Secret '{}' not found", name))
        }
    }

    struct FailingSecretStore {
        error_msg: String,
    }

    #[async_trait]
    impl SecretStoreProvider for FailingSecretStore {
        async fn get_secret(&self, _name: &str) -> anyhow::Result<String> {
            Err(anyhow!("{}", self.error_msg))
        }
    }

    #[tokio::test]
    async fn resolves_secret_value_from_provider() {
        let adapter = SecretStoreValueResolverAdapter::new(Arc::new(MockSecretStore {
            secrets: HashMap::from([("my-key".to_string(), "resolved-value".to_string())]),
        }));

        let result = adapter
            .resolve_to_string(&ConfigValue::Secret {
                name: "my-key".to_string(),
            })
            .await
            .expect("secret should resolve");

        assert_eq!(result, "resolved-value");
    }

    #[tokio::test]
    async fn propagates_provider_errors_as_secret_resolution_failed() {
        let adapter = SecretStoreValueResolverAdapter::new(Arc::new(FailingSecretStore {
            error_msg: "auth failed".to_string(),
        }));

        let error = adapter
            .resolve_to_string(&ConfigValue::Secret {
                name: "my-key".to_string(),
            })
            .await
            .expect_err("secret resolution should fail");

        match error {
            ResolverError::SecretResolutionFailed(message) => {
                assert_eq!(message, "Failed to resolve secret 'my-key': auth failed");
            }
            other => panic!("expected SecretResolutionFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_non_secret_variants() {
        let adapter = SecretStoreValueResolverAdapter::new(Arc::new(MockSecretStore {
            secrets: HashMap::new(),
        }));

        let static_error = adapter
            .resolve_to_string(&ConfigValue::Static("hello".to_string()))
            .await
            .expect_err("static value should be rejected");
        assert!(matches!(static_error, ResolverError::WrongResolverType));

        let env_error = adapter
            .resolve_to_string(&ConfigValue::EnvironmentVariable {
                name: "FOO".to_string(),
                default: None,
            })
            .await
            .expect_err("env var should be rejected");
        assert!(matches!(env_error, ResolverError::WrongResolverType));
    }

    #[tokio::test]
    async fn resolves_multiple_secret_names() {
        let adapter = SecretStoreValueResolverAdapter::new(Arc::new(MockSecretStore {
            secrets: HashMap::from([
                ("first-key".to_string(), "first-value".to_string()),
                ("second-key".to_string(), "second-value".to_string()),
            ]),
        }));

        let first = adapter
            .resolve_to_string(&ConfigValue::Secret {
                name: "first-key".to_string(),
            })
            .await
            .expect("first secret should resolve");
        let second = adapter
            .resolve_to_string(&ConfigValue::Secret {
                name: "second-key".to_string(),
            })
            .await
            .expect("second secret should resolve");

        assert_eq!(first, "first-value");
        assert_eq!(second, "second-value");
    }
}
