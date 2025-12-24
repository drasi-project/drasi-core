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

//! State Store Provider Plugin Trait
//!
//! This module provides the `StateStoreProvider` trait that allows plugins
//! (Sources, BootstrapProviders, and Reactions) to store and retrieve
//! runtime state that can persist across runs of DrasiLib.
//!
//! # Architecture
//!
//! The state store plugin system follows pure dependency inversion:
//! - **Lib** defines the `StateStoreProvider` trait and provides a default
//!   in-memory implementation (`MemoryStateStoreProvider`)
//! - **External plugins** (in `components/state_stores/`) implement this trait
//!   for persistent storage
//! - **Applications** optionally inject plugins into DrasiLib; if none provided,
//!   the in-memory default is used
//!
//! # Partitioning
//!
//! The state store supports partitioning via `StoreId`. Each plugin provides
//! a unique `StoreId` (typically the plugin's ID) when interacting with the
//! store. This ensures that different plugins don't interfere with each other's
//! state.
//!
//! # Usage
//!
//! ## Without a plugin (uses in-memory default)
//! ```ignore
//! let drasi = DrasiLib::builder()
//!     .build()?;
//! ```
//!
//! ## With an external plugin
//! ```ignore
//! use drasi_state_store_json::JsonStateStoreProvider;
//!
//! let state_store = JsonStateStoreProvider::new("/data/state");
//! let drasi = DrasiLib::builder()
//!     .with_state_store_provider(Arc::new(state_store))
//!     .build()?;
//! ```

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

/// Errors that can occur when interacting with a state store
#[derive(Error, Debug)]
pub enum StateStoreError {
    /// The requested key was not found in the store
    #[error("Key not found: {0}")]
    KeyNotFound(String),

    /// Failed to serialize or deserialize data
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Failed to read or write to the underlying storage
    #[error("Storage error: {0}")]
    StorageError(String),

    /// Generic error for other failures
    #[error("State store error: {0}")]
    Other(String),
}

/// Result type for state store operations
pub type StateStoreResult<T> = Result<T, StateStoreError>;

/// Trait defining the interface for state store providers.
///
/// State store providers allow plugins (Sources, BootstrapProviders, and Reactions)
/// to persist runtime state that survives restarts of DrasiLib.
///
/// # Thread Safety
///
/// Implementations must be thread-safe and support concurrent access from
/// multiple plugins.
///
/// # Partitioning
///
/// The state store uses `store_id` to partition data between different plugins.
/// Each plugin should use a unique `store_id` (typically the plugin's ID) to
/// avoid conflicts with other plugins.
///
/// # Example Implementation
///
/// ```ignore
/// use drasi_lib::plugin_core::StateStoreProvider;
/// use async_trait::async_trait;
///
/// pub struct MyStateStore {
///     // implementation fields
/// }
///
/// #[async_trait]
/// impl StateStoreProvider for MyStateStore {
///     async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
///         // implementation
///     }
///
///     async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
///         // implementation
///     }
///
///     async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
///         // implementation
///     }
///
///     // ... implement other methods
/// }
/// ```
#[async_trait]
pub trait StateStoreProvider: Send + Sync {
    /// Get a single value by key from a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `key` - The key to retrieve
    ///
    /// # Returns
    /// * `Ok(Some(value))` - The value was found
    /// * `Ok(None)` - The key doesn't exist in the store
    /// * `Err(e)` - An error occurred
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>>;

    /// Set a single value by key in a store partition.
    ///
    /// The state must be persisted before this method returns.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `key` - The key to set
    /// * `value` - The value to store
    ///
    /// # Returns
    /// * `Ok(())` - The value was successfully stored
    /// * `Err(e)` - An error occurred
    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()>;

    /// Delete a single key from a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `key` - The key to delete
    ///
    /// # Returns
    /// * `Ok(true)` - The key existed and was deleted
    /// * `Ok(false)` - The key didn't exist
    /// * `Err(e)` - An error occurred
    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool>;

    /// Check if a key exists in a store partition without retrieving its value.
    ///
    /// This is more efficient than `get()` when you only need to check existence,
    /// especially for large values.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `key` - The key to check
    ///
    /// # Returns
    /// * `Ok(true)` - The key exists
    /// * `Ok(false)` - The key doesn't exist
    /// * `Err(e)` - An error occurred
    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool>;

    /// Get multiple values by keys from a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `keys` - The keys to retrieve
    ///
    /// # Returns
    /// A HashMap mapping each found key to its value. Keys that don't exist
    /// are simply not included in the result.
    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>>;

    /// Set multiple key-value pairs in a store partition.
    ///
    /// All values must be persisted before this method returns.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `entries` - The key-value pairs to store (ownership is transferred)
    ///
    /// # Returns
    /// * `Ok(())` - All values were successfully stored
    /// * `Err(e)` - An error occurred (some values may have been stored)
    async fn set_many(
        &self,
        store_id: &str,
        entries: Vec<(String, Vec<u8>)>,
    ) -> StateStoreResult<()>;

    /// Delete multiple keys from a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier (typically the plugin ID)
    /// * `keys` - The keys to delete
    ///
    /// # Returns
    /// * `Ok(count)` - The number of keys that were deleted
    /// * `Err(e)` - An error occurred (some keys may have been deleted)
    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize>;

    /// Delete all data for a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier to clear
    ///
    /// # Returns
    /// * `Ok(count)` - The number of keys that were deleted
    /// * `Err(e)` - An error occurred
    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize>;

    /// List all keys in a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier
    ///
    /// # Returns
    /// A vector of all keys in the store partition
    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>>;

    /// Check if a store partition exists and has any data.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier to check
    ///
    /// # Returns
    /// * `Ok(true)` - The store partition exists and has at least one key
    /// * `Ok(false)` - The store partition doesn't exist or is empty
    /// * `Err(e)` - An error occurred while checking
    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool>;

    /// Get the number of keys in a store partition.
    ///
    /// # Arguments
    /// * `store_id` - The partition identifier
    ///
    /// # Returns
    /// * `Ok(count)` - The number of keys in the store
    /// * `Err(e)` - An error occurred
    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize>;

    /// Force pending writes to persistent storage.
    ///
    /// For in-memory stores, this is a no-op. For persistent stores, this
    /// ensures all data is durably written to disk.
    ///
    /// # Returns
    /// * `Ok(())` - Sync completed successfully
    /// * `Err(e)` - An error occurred during sync
    async fn sync(&self) -> StateStoreResult<()> {
        Ok(())
    }
}

/// In-memory implementation of StateStoreProvider.
///
/// This is the default state store provider used when no external provider
/// is configured. Data is stored in memory and does not persist across restarts.
///
/// # Thread Safety
///
/// This implementation is thread-safe and supports concurrent access from
/// multiple plugins.
///
/// # Usage
///
/// ```ignore
/// use drasi_lib::plugin_core::MemoryStateStoreProvider;
///
/// let provider = MemoryStateStoreProvider::new();
///
/// // Store some data
/// provider.set("my-plugin", "key1", b"value1".to_vec()).await?;
///
/// // Retrieve data
/// if let Some(value) = provider.get("my-plugin", "key1").await? {
///     println!("Value: {:?}", value);
/// }
/// ```
pub struct MemoryStateStoreProvider {
    /// Data storage: store_id -> (key -> value)
    stores: Arc<RwLock<HashMap<String, HashMap<String, Vec<u8>>>>>,
}

impl Default for MemoryStateStoreProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStateStoreProvider {
    /// Create a new in-memory state store provider.
    pub fn new() -> Self {
        Self {
            stores: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl StateStoreProvider for MemoryStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        let stores = self.stores.read().await;
        Ok(stores
            .get(store_id)
            .and_then(|store| store.get(key).cloned()))
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        let mut stores = self.stores.write().await;
        stores
            .entry(store_id.to_string())
            .or_default()
            .insert(key.to_string(), value);
        Ok(())
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        let mut stores = self.stores.write().await;
        if let Some(store) = stores.get_mut(store_id) {
            let existed = store.remove(key).is_some();
            // Clean up empty stores
            if store.is_empty() {
                stores.remove(store_id);
            }
            Ok(existed)
        } else {
            Ok(false)
        }
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        let stores = self.stores.read().await;
        Ok(stores
            .get(store_id)
            .is_some_and(|store| store.contains_key(key)))
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        let stores = self.stores.read().await;
        let mut result = HashMap::new();

        if let Some(store) = stores.get(store_id) {
            for key in keys {
                if let Some(value) = store.get(*key) {
                    result.insert((*key).to_string(), value.clone());
                }
            }
        }

        Ok(result)
    }

    async fn set_many(
        &self,
        store_id: &str,
        entries: Vec<(String, Vec<u8>)>,
    ) -> StateStoreResult<()> {
        let mut stores = self.stores.write().await;
        let store = stores.entry(store_id.to_string()).or_default();

        for (key, value) in entries {
            store.insert(key, value);
        }

        Ok(())
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        let mut stores = self.stores.write().await;
        let mut count = 0;

        if let Some(store) = stores.get_mut(store_id) {
            for key in keys {
                if store.remove(*key).is_some() {
                    count += 1;
                }
            }
            // Clean up empty stores
            if store.is_empty() {
                stores.remove(store_id);
            }
        }

        Ok(count)
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        let mut stores = self.stores.write().await;
        if let Some(store) = stores.remove(store_id) {
            Ok(store.len())
        } else {
            Ok(0)
        }
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        let stores = self.stores.read().await;
        Ok(stores
            .get(store_id)
            .map(|store| store.keys().cloned().collect())
            .unwrap_or_default())
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        let stores = self.stores.read().await;
        Ok(stores.get(store_id).is_some_and(|store| !store.is_empty()))
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        let stores = self.stores.read().await;
        Ok(stores.get(store_id).map(|store| store.len()).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_state_store_get_set() {
        let provider = MemoryStateStoreProvider::new();

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();

        // Get the value
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, Some(b"value1".to_vec()));

        // Get non-existent key
        let result = provider.get("store1", "nonexistent").await.unwrap();
        assert_eq!(result, None);

        // Get from non-existent store
        let result = provider.get("nonexistent", "key1").await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_memory_state_store_delete() {
        let provider = MemoryStateStoreProvider::new();

        // Set and delete
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        let deleted = provider.delete("store1", "key1").await.unwrap();
        assert!(deleted);

        // Verify deletion
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, None);

        // Delete non-existent key
        let deleted = provider.delete("store1", "nonexistent").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_memory_state_store_get_many() {
        let provider = MemoryStateStoreProvider::new();

        // Set multiple values
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key3", b"value3".to_vec())
            .await
            .unwrap();

        // Get multiple (including non-existent)
        let result = provider
            .get_many("store1", &["key1", "key2", "nonexistent"])
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("key1"), Some(&b"value1".to_vec()));
        assert_eq!(result.get("key2"), Some(&b"value2".to_vec()));
        assert_eq!(result.get("nonexistent"), None);
    }

    #[tokio::test]
    async fn test_memory_state_store_set_many() {
        let provider = MemoryStateStoreProvider::new();

        // Set multiple values (new signature takes ownership)
        provider
            .set_many(
                "store1",
                vec![
                    ("key1".to_string(), b"value1".to_vec()),
                    ("key2".to_string(), b"value2".to_vec()),
                ],
            )
            .await
            .unwrap();

        // Verify
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, Some(b"value1".to_vec()));
        let result = provider.get("store1", "key2").await.unwrap();
        assert_eq!(result, Some(b"value2".to_vec()));
    }

    #[tokio::test]
    async fn test_memory_state_store_delete_many() {
        let provider = MemoryStateStoreProvider::new();

        // Set multiple values (new signature takes ownership)
        provider
            .set_many(
                "store1",
                vec![
                    ("key1".to_string(), b"value1".to_vec()),
                    ("key2".to_string(), b"value2".to_vec()),
                    ("key3".to_string(), b"value3".to_vec()),
                ],
            )
            .await
            .unwrap();

        // Delete some
        let count = provider
            .delete_many("store1", &["key1", "key2", "nonexistent"])
            .await
            .unwrap();
        assert_eq!(count, 2);

        // Verify
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, None);
        let result = provider.get("store1", "key3").await.unwrap();
        assert_eq!(result, Some(b"value3".to_vec()));
    }

    #[tokio::test]
    async fn test_memory_state_store_clear_store() {
        let provider = MemoryStateStoreProvider::new();

        // Set values in multiple stores
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();
        provider
            .set("store2", "key1", b"value1".to_vec())
            .await
            .unwrap();

        // Clear store1
        let count = provider.clear_store("store1").await.unwrap();
        assert_eq!(count, 2);

        // Verify store1 is cleared
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, None);

        // Verify store2 is intact
        let result = provider.get("store2", "key1").await.unwrap();
        assert_eq!(result, Some(b"value1".to_vec()));

        // Clear non-existent store
        let count = provider.clear_store("nonexistent").await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_memory_state_store_list_keys() {
        let provider = MemoryStateStoreProvider::new();

        // Set values
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();

        // List keys
        let mut keys = provider.list_keys("store1").await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["key1", "key2"]);

        // List keys from non-existent store
        let keys = provider.list_keys("nonexistent").await.unwrap();
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn test_memory_state_store_store_exists() {
        let provider = MemoryStateStoreProvider::new();

        // Non-existent store
        assert!(!provider.store_exists("store1").await.unwrap());

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert!(provider.store_exists("store1").await.unwrap());

        // Delete the value (store should be cleaned up)
        provider.delete("store1", "key1").await.unwrap();
        assert!(!provider.store_exists("store1").await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_state_store_partitioning() {
        let provider = MemoryStateStoreProvider::new();

        // Set same key in different stores
        provider
            .set("store1", "key", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store2", "key", b"value2".to_vec())
            .await
            .unwrap();

        // Verify they don't interfere
        let result1 = provider.get("store1", "key").await.unwrap();
        let result2 = provider.get("store2", "key").await.unwrap();
        assert_eq!(result1, Some(b"value1".to_vec()));
        assert_eq!(result2, Some(b"value2".to_vec()));
    }

    #[tokio::test]
    async fn test_memory_state_store_empty_store_cleanup() {
        let provider = MemoryStateStoreProvider::new();

        // Set and delete to create then empty a store
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert!(provider.store_exists("store1").await.unwrap());

        provider.delete("store1", "key1").await.unwrap();
        assert!(!provider.store_exists("store1").await.unwrap());

        // Also test with delete_many (new signature takes ownership)
        provider
            .set_many(
                "store2",
                vec![
                    ("key1".to_string(), b"value1".to_vec()),
                    ("key2".to_string(), b"value2".to_vec()),
                ],
            )
            .await
            .unwrap();
        assert!(provider.store_exists("store2").await.unwrap());

        provider
            .delete_many("store2", &["key1", "key2"])
            .await
            .unwrap();
        assert!(!provider.store_exists("store2").await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_state_store_contains_key() {
        let provider = MemoryStateStoreProvider::new();

        // Key doesn't exist
        assert!(!provider.contains_key("store1", "key1").await.unwrap());

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();

        // Key now exists
        assert!(provider.contains_key("store1", "key1").await.unwrap());

        // Different key doesn't exist
        assert!(!provider.contains_key("store1", "key2").await.unwrap());

        // Different store doesn't have the key
        assert!(!provider.contains_key("store2", "key1").await.unwrap());

        // Delete the key
        provider.delete("store1", "key1").await.unwrap();
        assert!(!provider.contains_key("store1", "key1").await.unwrap());
    }

    #[tokio::test]
    async fn test_memory_state_store_key_count() {
        let provider = MemoryStateStoreProvider::new();

        // Empty store has 0 keys
        assert_eq!(provider.key_count("store1").await.unwrap(), 0);

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);

        // Set another value
        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 2);

        // Different store is still 0
        assert_eq!(provider.key_count("store2").await.unwrap(), 0);

        // Delete a key
        provider.delete("store1", "key1").await.unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);

        // Clear the store
        provider.clear_store("store1").await.unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_memory_state_store_sync() {
        let provider = MemoryStateStoreProvider::new();

        // sync is a no-op for memory provider but should succeed
        provider.sync().await.unwrap();

        // Set some data and sync again
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider.sync().await.unwrap();
    }
}
