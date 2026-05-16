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

//! Secret store providers for resolving named secrets at runtime.
//!
//! A [`SecretStoreProvider`] resolves named secret references (e.g., `"DB_PASSWORD"`)
//! into their actual string values. Plugin configuration DTOs use
//! [`ConfigValue::Secret`](drasi_plugin_sdk::ConfigValue) references which are resolved
//! through the configured secret store during plugin initialization.
//!
//! # Architecture
//!
//! The secret store plugin system follows pure dependency inversion:
//! - **drasi-lib** defines the [`SecretStoreProvider`] trait
//! - **Plugin crates** (in `components/secret_stores/`) implement this trait
//!   for specific backends (file, OS keyring, Azure Key Vault, etc.)
//! - **Applications** inject a provider into DrasiLib via the builder
//!
//! Secret stores are initialized **before** any source/reaction/bootstrap plugins,
//! because those plugins need resolved secrets during their `create_*` calls.
//!
//! # Built-in Implementations
//!
//! - [`MemorySecretStoreProvider`] — In-memory store for testing
//!
//! # Usage
//!
//! ## With a file-based secret store plugin
//! ```ignore
//! use drasi_secret_store_file::FileSecretStoreProvider;
//!
//! let secret_store = FileSecretStoreProvider::new("/path/to/secrets.json").await?;
//! let drasi = DrasiLib::builder()
//!     .with_secret_store_provider(Arc::new(secret_store))
//!     .build()
//!     .await?;
//! ```
//!
//! ## With the in-memory provider (testing)
//! ```ignore
//! use drasi_lib::secret_store::MemorySecretStoreProvider;
//!
//! let store = MemorySecretStoreProvider::new()
//!     .with_secret("DB_PASSWORD", "hunter2")
//!     .with_secret("API_KEY", "abc123");
//!
//! let drasi = DrasiLib::builder()
//!     .with_secret_store_provider(Arc::new(store))
//!     .build()
//!     .await?;
//! ```

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Trait for secret store providers that resolve named secrets at runtime.
///
/// Implementations fetch secret values from a backend (file, OS keyring,
/// cloud vault, etc.) and return them as strings. The Drasi framework calls
/// this trait during plugin initialization to resolve `ConfigValue::Secret`
/// references in plugin configuration DTOs.
///
/// This is a plugin trait (Layer 3) — implementations return `anyhow::Result`
/// and should use `.context()` for error chains. The framework wraps these
/// into `DrasiError` at the public API boundary.
///
/// # Important
///
/// Secret store providers are initialized before any other plugins. Their own
/// configuration must use static values or environment variables — not
/// `ConfigValue::Secret` references (which would create a circular dependency).
#[async_trait]
pub trait SecretStoreProvider: Send + Sync {
    /// Resolve a named secret to its string value.
    ///
    /// # Arguments
    /// * `name` - The secret name (e.g., `"DB_PASSWORD"`, `"API_KEY"`)
    ///
    /// # Returns
    /// * `Ok(value)` - The secret's string value
    /// * `Err(e)` - The secret could not be resolved (not found, access denied, etc.)
    async fn get_secret(&self, name: &str) -> Result<String>;
}

/// In-memory secret store for testing.
///
/// Stores secrets in a `HashMap` and returns them on demand. Secrets can be
/// added at construction time via the builder pattern or at runtime via
/// `set_secret()`.
///
/// # Example
///
/// ```
/// use drasi_lib::secret_store::MemorySecretStoreProvider;
///
/// let store = MemorySecretStoreProvider::new()
///     .with_secret("DB_PASSWORD", "hunter2")
///     .with_secret("API_KEY", "abc123");
/// ```
pub struct MemorySecretStoreProvider {
    secrets: RwLock<HashMap<String, String>>,
}

impl MemorySecretStoreProvider {
    /// Create a new empty in-memory secret store.
    pub fn new() -> Self {
        Self {
            secrets: RwLock::new(HashMap::new()),
        }
    }

    /// Add a secret during construction (builder pattern).
    #[must_use]
    pub fn with_secret(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        // Use try_write since we know we're the only owner during construction
        self.secrets
            .try_write()
            .expect("MemorySecretStoreProvider should not be shared during construction")
            .insert(name.into(), value.into());
        self
    }

    /// Add or update a secret at runtime.
    pub async fn set_secret(&self, name: impl Into<String>, value: impl Into<String>) {
        self.secrets.write().await.insert(name.into(), value.into());
    }

    /// Remove a secret at runtime.
    pub async fn remove_secret(&self, name: &str) -> Option<String> {
        self.secrets.write().await.remove(name)
    }
}

impl Default for MemorySecretStoreProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStoreProvider for MemorySecretStoreProvider {
    async fn get_secret(&self, name: &str) -> Result<String> {
        self.secrets
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Secret not found: {name}"))
    }
}

impl std::fmt::Debug for MemorySecretStoreProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySecretStoreProvider")
            .field("secrets", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_store_get_secret() {
        let store = MemorySecretStoreProvider::new()
            .with_secret("DB_PASSWORD", "hunter2")
            .with_secret("API_KEY", "abc123");

        assert_eq!(store.get_secret("DB_PASSWORD").await.unwrap(), "hunter2");
        assert_eq!(store.get_secret("API_KEY").await.unwrap(), "abc123");
    }

    #[tokio::test]
    async fn test_memory_store_missing_secret() {
        let store = MemorySecretStoreProvider::new();
        let result = store.get_secret("NONEXISTENT").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Secret not found"));
    }

    #[tokio::test]
    async fn test_memory_store_set_and_remove() {
        let store = MemorySecretStoreProvider::new();
        store.set_secret("KEY", "value").await;
        assert_eq!(store.get_secret("KEY").await.unwrap(), "value");

        let removed = store.remove_secret("KEY").await;
        assert_eq!(removed, Some("value".to_string()));
        assert!(store.get_secret("KEY").await.is_err());
    }

    #[test]
    fn test_memory_store_debug_redacts() {
        let store = MemorySecretStoreProvider::new().with_secret("KEY", "secret_value");
        let debug = format!("{store:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret_value"));
    }

    #[test]
    fn test_memory_store_default() {
        let store = MemorySecretStoreProvider::default();
        // Should be empty
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(store.get_secret("anything")).is_err());
    }
}
