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

//! JSON file-based state store provider implementation.

use async_trait::async_trait;
use drasi_lib::plugin_core::{StateStoreError, StateStoreProvider, StateStoreResult};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Internal representation of store data in JSON files.
/// Values are stored as base64-encoded strings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoreData {
    #[serde(flatten)]
    entries: HashMap<String, String>,
}

impl StoreData {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// JSON file-based state store provider.
///
/// This provider stores state data in JSON files, with one file per store partition.
/// Data is persisted to disk and survives restarts.
///
/// # Thread Safety
///
/// This implementation uses internal locking to ensure thread-safe access to the
/// in-memory cache and file operations. Multiple plugins can safely read and write
/// concurrently.
///
/// # File Format
///
/// Each store is saved as a JSON file with the following format:
/// ```json
/// {
///   "key1": "base64_encoded_value",
///   "key2": "base64_encoded_value"
/// }
/// ```
///
/// Values are base64-encoded because they are arbitrary byte sequences.
///
/// # Example
///
/// ```ignore
/// use drasi_state_store_json::JsonStateStoreProvider;
/// use std::sync::Arc;
///
/// let provider = JsonStateStoreProvider::new("/data/state")?;
///
/// // Use with DrasiLib
/// let drasi = DrasiLib::builder()
///     .with_state_store_provider(Arc::new(provider))
///     .build()
///     .await?;
/// ```
pub struct JsonStateStoreProvider {
    /// Base directory for JSON files
    directory: PathBuf,
    /// In-memory cache of store data
    cache: Arc<RwLock<HashMap<String, StoreData>>>,
}

impl JsonStateStoreProvider {
    /// Create a new JSON state store provider.
    ///
    /// The provider will create the directory if it doesn't exist and load
    /// any existing JSON files as initial state.
    ///
    /// # Arguments
    ///
    /// * `directory` - Base directory for storing JSON files
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = JsonStateStoreProvider::new("/data/state")?;
    /// ```
    pub fn new<P: Into<PathBuf>>(directory: P) -> Result<Self, std::io::Error> {
        let directory = directory.into();

        // Create directory if it doesn't exist
        std::fs::create_dir_all(&directory)?;

        // Load existing files into a regular HashMap first
        let cache_data = Self::load_existing_files_sync(&directory)?;

        let provider = Self {
            directory,
            cache: Arc::new(RwLock::new(cache_data)),
        };

        Ok(provider)
    }

    /// Load existing JSON files from the directory into a HashMap.
    /// This is a synchronous operation used during construction.
    fn load_existing_files_sync(
        directory: &PathBuf,
    ) -> Result<HashMap<String, StoreData>, std::io::Error> {
        let mut cache_data = HashMap::new();
        let entries = std::fs::read_dir(directory)?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                if let Some(stem) = path.file_stem() {
                    let store_id = stem.to_string_lossy().to_string();
                    match Self::load_store_from_file(&path) {
                        Ok(data) => {
                            cache_data.insert(store_id.clone(), data);
                            info!("Loaded state store '{store_id}' from file");
                        }
                        Err(e) => {
                            warn!("Failed to load state store from {path:?}: {e}");
                        }
                    }
                }
            }
        }

        Ok(cache_data)
    }

    /// Load store data from a JSON file.
    fn load_store_from_file(path: &PathBuf) -> Result<StoreData, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        let data: StoreData =
            serde_json::from_str(&content).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(data)
    }

    /// Get the file path for a store.
    fn get_store_path(&self, store_id: &str) -> PathBuf {
        self.directory.join(format!("{store_id}.json"))
    }

    /// Save store data to a JSON file.
    async fn save_store(&self, store_id: &str, data: &StoreData) -> StateStoreResult<()> {
        let path = self.get_store_path(store_id);

        // If the store is empty, delete the file instead
        if data.is_empty() {
            return self.delete_store_file(store_id).await;
        }

        let json = serde_json::to_string_pretty(data).map_err(|e| {
            StateStoreError::SerializationError(format!("Failed to serialize store data: {e}"))
        })?;

        tokio::fs::write(&path, json).await.map_err(|e| {
            StateStoreError::StorageError(format!("Failed to write store file {path:?}: {e}"))
        })?;

        debug!("Saved state store '{store_id}' to {path:?}");
        Ok(())
    }

    /// Delete a store's JSON file.
    async fn delete_store_file(&self, store_id: &str) -> StateStoreResult<()> {
        let path = self.get_store_path(store_id);

        if path.exists() {
            tokio::fs::remove_file(&path).await.map_err(|e| {
                StateStoreError::StorageError(format!("Failed to delete store file {path:?}: {e}"))
            })?;
            debug!("Deleted state store file for '{store_id}'");
        }

        Ok(())
    }

    /// Encode a value to base64 for JSON storage.
    fn encode_value(value: &[u8]) -> String {
        use serde::ser::Serialize;
        // Use raw base64 encoding
        base64_encode(value)
    }

    /// Decode a base64 value from JSON storage.
    fn decode_value(encoded: &str) -> StateStoreResult<Vec<u8>> {
        base64_decode(encoded).map_err(|e| {
            StateStoreError::SerializationError(format!("Failed to decode value: {e}"))
        })
    }
}

/// Simple base64 encoding (no external dependency)
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut result = String::new();
    let chunks = data.chunks(3);

    for chunk in chunks {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        result.push(ALPHABET[(b0 >> 2) & 0x3f] as char);
        result.push(ALPHABET[((b0 << 4) | (b1 >> 4)) & 0x3f] as char);

        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 << 2) | (b2 >> 6)) & 0x3f] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Simple base64 decoding
fn base64_decode(encoded: &str) -> Result<Vec<u8>, String> {
    const DECODE_TABLE: [i8; 128] = [
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1,
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 62, -1, -1,
        -1, 63, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, -1, -1, -1, -1, -1, -1, -1, 0, 1, 2, 3, 4,
        5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, -1, -1, -1,
        -1, -1, -1, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45,
        46, 47, 48, 49, 50, 51, -1, -1, -1, -1, -1,
    ];

    let encoded = encoded.trim_end_matches('=');
    let mut result = Vec::with_capacity(encoded.len() * 3 / 4);

    let chars: Vec<u8> = encoded
        .chars()
        .filter_map(|c| {
            let b = c as usize;
            if b < 128 {
                let decoded = DECODE_TABLE[b];
                if decoded >= 0 {
                    return Some(decoded as u8);
                }
            }
            None
        })
        .collect();

    for chunk in chars.chunks(4) {
        if chunk.len() < 2 {
            break;
        }

        let b0 = chunk[0] as usize;
        let b1 = chunk[1] as usize;
        result.push(((b0 << 2) | (b1 >> 4)) as u8);

        if chunk.len() > 2 {
            let b2 = chunk[2] as usize;
            result.push((((b1 & 0x0f) << 4) | (b2 >> 2)) as u8);

            if chunk.len() > 3 {
                let b3 = chunk[3] as usize;
                result.push((((b2 & 0x03) << 6) | b3) as u8);
            }
        }
    }

    Ok(result)
}

#[async_trait]
impl StateStoreProvider for JsonStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        let cache = self.cache.read().await;

        if let Some(store) = cache.get(store_id) {
            if let Some(encoded) = store.entries.get(key) {
                let value = Self::decode_value(encoded)?;
                return Ok(Some(value));
            }
        }

        Ok(None)
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        let encoded = Self::encode_value(&value);

        let store_data = {
            let mut cache = self.cache.write().await;
            let store = cache.entry(store_id.to_string()).or_default();
            store.entries.insert(key.to_string(), encoded);
            store.clone()
        };

        // Save to disk
        self.save_store(store_id, &store_data).await?;

        Ok(())
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        let (existed, store_data) = {
            let mut cache = self.cache.write().await;

            if let Some(store) = cache.get_mut(store_id) {
                let existed = store.entries.remove(key).is_some();

                // If store is empty, remove it from cache
                if store.is_empty() {
                    cache.remove(store_id);
                    (existed, None)
                } else {
                    (existed, Some(store.clone()))
                }
            } else {
                (false, None)
            }
        };

        // Save to disk (or delete file if empty)
        if existed {
            if let Some(data) = store_data {
                self.save_store(store_id, &data).await?;
            } else {
                self.delete_store_file(store_id).await?;
            }
        }

        Ok(existed)
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        let cache = self.cache.read().await;
        let mut result = HashMap::new();

        if let Some(store) = cache.get(store_id) {
            for key in keys {
                if let Some(encoded) = store.entries.get(*key) {
                    let value = Self::decode_value(encoded)?;
                    result.insert((*key).to_string(), value);
                }
            }
        }

        Ok(result)
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, Vec<u8>)]) -> StateStoreResult<()> {
        let store_data = {
            let mut cache = self.cache.write().await;
            let store = cache.entry(store_id.to_string()).or_default();

            for (key, value) in entries {
                let encoded = Self::encode_value(value);
                store.entries.insert((*key).to_string(), encoded);
            }

            store.clone()
        };

        // Save to disk
        self.save_store(store_id, &store_data).await?;

        Ok(())
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        let (count, store_data) = {
            let mut cache = self.cache.write().await;
            let mut count = 0;

            if let Some(store) = cache.get_mut(store_id) {
                for key in keys {
                    if store.entries.remove(*key).is_some() {
                        count += 1;
                    }
                }

                // If store is empty, remove it from cache
                if store.is_empty() {
                    cache.remove(store_id);
                    (count, None)
                } else {
                    (count, Some(store.clone()))
                }
            } else {
                (0, None)
            }
        };

        // Save to disk (or delete file if empty)
        if count > 0 {
            if let Some(data) = store_data {
                self.save_store(store_id, &data).await?;
            } else {
                self.delete_store_file(store_id).await?;
            }
        }

        Ok(count)
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        let count = {
            let mut cache = self.cache.write().await;
            if let Some(store) = cache.remove(store_id) {
                store.entries.len()
            } else {
                0
            }
        };

        // Delete file
        self.delete_store_file(store_id).await?;

        Ok(count)
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        let cache = self.cache.read().await;

        Ok(cache
            .get(store_id)
            .map(|store| store.entries.keys().cloned().collect())
            .unwrap_or_default())
    }

    async fn store_exists(&self, store_id: &str) -> bool {
        let cache = self.cache.read().await;
        cache.get(store_id).is_some_and(|store| !store.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_base64_encode_decode() {
        let test_cases = vec![
            b"hello world".to_vec(),
            b"".to_vec(),
            vec![0, 1, 2, 3, 255, 254, 253],
            b"test".to_vec(),
            b"a".to_vec(),
            b"ab".to_vec(),
        ];

        for data in test_cases {
            let encoded = base64_encode(&data);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(data, decoded, "Failed for data: {:?}", data);
        }
    }

    #[tokio::test]
    async fn test_json_state_store_get_set() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

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

        // Verify file exists
        let file_path = temp_dir.path().join("store1.json");
        assert!(file_path.exists());
    }

    #[tokio::test]
    async fn test_json_state_store_delete() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

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

        // File should be deleted since store is empty
        let file_path = temp_dir.path().join("store1.json");
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn test_json_state_store_persistence() {
        let temp_dir = TempDir::new().unwrap();

        // Create provider and set data
        {
            let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();
            provider
                .set("store1", "key1", b"value1".to_vec())
                .await
                .unwrap();
            provider
                .set("store1", "key2", b"value2".to_vec())
                .await
                .unwrap();
        }

        // Create new provider and verify data is loaded
        {
            let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();
            let result = provider.get("store1", "key1").await.unwrap();
            assert_eq!(result, Some(b"value1".to_vec()));
            let result = provider.get("store1", "key2").await.unwrap();
            assert_eq!(result, Some(b"value2".to_vec()));
        }
    }

    #[tokio::test]
    async fn test_json_state_store_multiple_stores() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

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

        // Verify separate files
        assert!(temp_dir.path().join("store1.json").exists());
        assert!(temp_dir.path().join("store2.json").exists());
    }

    #[tokio::test]
    async fn test_json_state_store_clear_store() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

        // Set values
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();

        // Clear store
        let count = provider.clear_store("store1").await.unwrap();
        assert_eq!(count, 2);

        // Verify deletion
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, None);

        // File should be deleted
        let file_path = temp_dir.path().join("store1.json");
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn test_json_state_store_list_keys() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

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
    }

    #[tokio::test]
    async fn test_json_state_store_store_exists() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

        // Non-existent store
        assert!(!provider.store_exists("store1").await);

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert!(provider.store_exists("store1").await);

        // Delete the value
        provider.delete("store1", "key1").await.unwrap();
        assert!(!provider.store_exists("store1").await);
    }

    #[tokio::test]
    async fn test_json_state_store_get_many() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

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
    }

    #[tokio::test]
    async fn test_json_state_store_set_many() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

        // Set multiple values
        provider
            .set_many(
                "store1",
                &[("key1", b"value1".to_vec()), ("key2", b"value2".to_vec())],
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
    async fn test_json_state_store_delete_many() {
        let temp_dir = TempDir::new().unwrap();
        let provider = JsonStateStoreProvider::new(temp_dir.path()).unwrap();

        // Set multiple values
        provider
            .set_many(
                "store1",
                &[
                    ("key1", b"value1".to_vec()),
                    ("key2", b"value2".to_vec()),
                    ("key3", b"value3".to_vec()),
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
}
