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

//! Redb-based state store provider implementation.

use async_trait::async_trait;
use drasi_lib::plugin_core::{StateStoreError, StateStoreProvider, StateStoreResult};
use log::{debug, info};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Table definition for storing key-value pairs.
/// Each store_id gets its own table with string keys and byte array values.
const fn table_definition(
    name: &'static str,
) -> TableDefinition<'static, &'static str, &'static [u8]> {
    TableDefinition::new(name)
}

/// Redb-based state store provider.
///
/// This provider stores state data in a redb database file with atomic transactions.
/// Each store partition (identified by `store_id`) gets its own table.
///
/// # Thread Safety
///
/// This implementation is thread-safe and supports concurrent access from
/// multiple plugins through redb's MVCC (multi-version concurrency control).
///
/// # Atomicity
///
/// All read and write operations are atomic. Batch operations (set_many, delete_many)
/// are performed in a single transaction.
///
/// # Example
///
/// ```ignore
/// use drasi_state_store_redb::RedbStateStoreProvider;
/// use std::sync::Arc;
///
/// let provider = RedbStateStoreProvider::new("/data/state.redb")?;
///
/// // Use with DrasiLib
/// let drasi = DrasiLib::builder()
///     .with_state_store_provider(Arc::new(provider))
///     .build()
///     .await?;
/// ```
pub struct RedbStateStoreProvider {
    /// The redb database instance
    db: Arc<Database>,
    /// Cache of known table names for quick lookups
    known_tables: Arc<RwLock<std::collections::HashSet<String>>>,
}

impl RedbStateStoreProvider {
    /// Create a new redb state store provider.
    ///
    /// This will create a new database file if it doesn't exist, or open
    /// an existing one.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the redb database file
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be created or opened.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = RedbStateStoreProvider::new("/data/state.redb")?;
    /// ```
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, StateStoreError> {
        let db = Database::create(path.as_ref()).map_err(|e| {
            StateStoreError::StorageError(format!("Failed to create/open redb database: {e}"))
        })?;

        info!("Opened redb state store at {:?}", path.as_ref());

        Ok(Self {
            db: Arc::new(db),
            known_tables: Arc::new(RwLock::new(std::collections::HashSet::new())),
        })
    }

    /// Get the table name for a store_id.
    /// Table names are prefixed with "store_" to avoid conflicts.
    fn table_name(store_id: &str) -> String {
        format!("store_{store_id}")
    }
}

#[async_trait]
impl StateStoreProvider for RedbStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        let table_name = Self::table_name(store_id);

        let read_txn = self.db.begin_read().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin read transaction: {e}"))
        })?;

        // Try to open the table - it may not exist yet
        let table_def: TableDefinition<&str, &[u8]> =
            TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

        match read_txn.open_table(table_def) {
            Ok(table) => {
                let result = table.get(key).map_err(|e| {
                    StateStoreError::StorageError(format!("Failed to get value: {e}"))
                })?;

                Ok(result.map(|v| v.value().to_vec()))
            }
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(e) => Err(StateStoreError::StorageError(format!(
                "Failed to open table: {e}"
            ))),
        }
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        let table_name = Self::table_name(store_id);

        let write_txn = self.db.begin_write().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin write transaction: {e}"))
        })?;

        {
            let table_def: TableDefinition<&str, &[u8]> =
                TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

            let mut table = write_txn
                .open_table(table_def)
                .map_err(|e| StateStoreError::StorageError(format!("Failed to open table: {e}")))?;

            table.insert(key, value.as_slice()).map_err(|e| {
                StateStoreError::StorageError(format!("Failed to insert value: {e}"))
            })?;
        }

        write_txn.commit().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to commit transaction: {e}"))
        })?;

        // Track the table as known
        self.known_tables.write().await.insert(table_name);

        debug!("Set key '{key}' in store '{store_id}'");
        Ok(())
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        let table_name = Self::table_name(store_id);

        let write_txn = self.db.begin_write().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin write transaction: {e}"))
        })?;

        let existed = {
            let table_def: TableDefinition<&str, &[u8]> =
                TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

            match write_txn.open_table(table_def) {
                Ok(mut table) => {
                    let result = table.remove(key).map_err(|e| {
                        StateStoreError::StorageError(format!("Failed to remove key: {e}"))
                    })?;
                    result.is_some()
                }
                Err(redb::TableError::TableDoesNotExist(_)) => false,
                Err(e) => {
                    return Err(StateStoreError::StorageError(format!(
                        "Failed to open table: {e}"
                    )))
                }
            }
        };

        write_txn.commit().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to commit transaction: {e}"))
        })?;

        if existed {
            debug!("Deleted key '{key}' from store '{store_id}'");
        }

        Ok(existed)
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        let table_name = Self::table_name(store_id);
        let mut result = HashMap::new();

        let read_txn = self.db.begin_read().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin read transaction: {e}"))
        })?;

        let table_def: TableDefinition<&str, &[u8]> =
            TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

        match read_txn.open_table(table_def) {
            Ok(table) => {
                for key in keys {
                    if let Some(value) = table.get(*key).map_err(|e| {
                        StateStoreError::StorageError(format!("Failed to get value: {e}"))
                    })? {
                        result.insert((*key).to_string(), value.value().to_vec());
                    }
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {
                // Table doesn't exist, return empty result
            }
            Err(e) => {
                return Err(StateStoreError::StorageError(format!(
                    "Failed to open table: {e}"
                )))
            }
        }

        Ok(result)
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, Vec<u8>)]) -> StateStoreResult<()> {
        let table_name = Self::table_name(store_id);

        let write_txn = self.db.begin_write().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin write transaction: {e}"))
        })?;

        {
            let table_def: TableDefinition<&str, &[u8]> =
                TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

            let mut table = write_txn
                .open_table(table_def)
                .map_err(|e| StateStoreError::StorageError(format!("Failed to open table: {e}")))?;

            for (key, value) in entries {
                table.insert(*key, value.as_slice()).map_err(|e| {
                    StateStoreError::StorageError(format!("Failed to insert value: {e}"))
                })?;
            }
        }

        write_txn.commit().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to commit transaction: {e}"))
        })?;

        // Track the table as known
        self.known_tables.write().await.insert(table_name);

        debug!("Set {} entries in store '{store_id}'", entries.len());
        Ok(())
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        let table_name = Self::table_name(store_id);

        let write_txn = self.db.begin_write().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin write transaction: {e}"))
        })?;

        let count = {
            let table_def: TableDefinition<&str, &[u8]> =
                TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

            match write_txn.open_table(table_def) {
                Ok(mut table) => {
                    let mut count = 0;
                    for key in keys {
                        let result = table.remove(*key).map_err(|e| {
                            StateStoreError::StorageError(format!("Failed to remove key: {e}"))
                        })?;
                        if result.is_some() {
                            count += 1;
                        }
                    }
                    count
                }
                Err(redb::TableError::TableDoesNotExist(_)) => 0,
                Err(e) => {
                    return Err(StateStoreError::StorageError(format!(
                        "Failed to open table: {e}"
                    )))
                }
            }
        };

        write_txn.commit().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to commit transaction: {e}"))
        })?;

        debug!("Deleted {count} keys from store '{store_id}'");
        Ok(count)
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        let table_name = Self::table_name(store_id);

        let write_txn = self.db.begin_write().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin write transaction: {e}"))
        })?;

        let count = {
            let table_def: TableDefinition<&str, &[u8]> =
                TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

            match write_txn.open_table(table_def) {
                Ok(table) => {
                    // Count entries first
                    let count = table.len().map_err(|e| {
                        StateStoreError::StorageError(format!("Failed to count entries: {e}"))
                    })?;
                    count as usize
                }
                Err(redb::TableError::TableDoesNotExist(_)) => 0,
                Err(e) => {
                    return Err(StateStoreError::StorageError(format!(
                        "Failed to open table: {e}"
                    )))
                }
            }
        };

        // Delete the table itself
        if count > 0 {
            let table_def: TableDefinition<&str, &[u8]> =
                TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

            write_txn.delete_table(table_def).map_err(|e| {
                StateStoreError::StorageError(format!("Failed to delete table: {e}"))
            })?;
        }

        write_txn.commit().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to commit transaction: {e}"))
        })?;

        // Remove from known tables
        self.known_tables.write().await.remove(&table_name);

        debug!("Cleared store '{store_id}' ({count} entries)");
        Ok(count)
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        let table_name = Self::table_name(store_id);
        let mut keys = Vec::new();

        let read_txn = self.db.begin_read().map_err(|e| {
            StateStoreError::StorageError(format!("Failed to begin read transaction: {e}"))
        })?;

        let table_def: TableDefinition<&str, &[u8]> =
            TableDefinition::new(Box::leak(table_name.clone().into_boxed_str()));

        match read_txn.open_table(table_def) {
            Ok(table) => {
                let iter = table.iter().map_err(|e| {
                    StateStoreError::StorageError(format!("Failed to iterate table: {e}"))
                })?;

                for entry in iter {
                    let (key, _) = entry.map_err(|e| {
                        StateStoreError::StorageError(format!("Failed to read entry: {e}"))
                    })?;
                    keys.push(key.value().to_string());
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {
                // Table doesn't exist, return empty list
            }
            Err(e) => {
                return Err(StateStoreError::StorageError(format!(
                    "Failed to open table: {e}"
                )))
            }
        }

        Ok(keys)
    }

    async fn store_exists(&self, store_id: &str) -> bool {
        let table_name = Self::table_name(store_id);

        let read_txn = match self.db.begin_read() {
            Ok(txn) => txn,
            Err(_) => return false,
        };

        let table_def: TableDefinition<&str, &[u8]> =
            TableDefinition::new(Box::leak(table_name.into_boxed_str()));

        match read_txn.open_table(table_def) {
            Ok(table) => {
                // Check if the table has any entries
                matches!(table.len(), Ok(len) if len > 0)
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_redb_state_store_get_set() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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
    async fn test_redb_state_store_delete() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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
    async fn test_redb_state_store_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // Create provider and set data
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();
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
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();
            let result = provider.get("store1", "key1").await.unwrap();
            assert_eq!(result, Some(b"value1".to_vec()));
            let result = provider.get("store1", "key2").await.unwrap();
            assert_eq!(result, Some(b"value2".to_vec()));
        }
    }

    #[tokio::test]
    async fn test_redb_state_store_multiple_stores() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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
    async fn test_redb_state_store_clear_store() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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

        // Store should not exist anymore
        assert!(!provider.store_exists("store1").await);
    }

    #[tokio::test]
    async fn test_redb_state_store_list_keys() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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
    async fn test_redb_state_store_store_exists() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Non-existent store
        assert!(!provider.store_exists("store1").await);

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert!(provider.store_exists("store1").await);

        // Clear the store
        provider.clear_store("store1").await.unwrap();
        assert!(!provider.store_exists("store1").await);
    }

    #[tokio::test]
    async fn test_redb_state_store_get_many() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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
    async fn test_redb_state_store_set_many() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Set multiple values atomically
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
    async fn test_redb_state_store_delete_many() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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

        // Delete some atomically
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
    async fn test_redb_state_store_binary_values() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Store binary data
        let binary_data: Vec<u8> = vec![0, 1, 2, 3, 255, 254, 253, 0];
        provider
            .set("store1", "binary", binary_data.clone())
            .await
            .unwrap();

        // Retrieve and verify
        let result = provider.get("store1", "binary").await.unwrap();
        assert_eq!(result, Some(binary_data));
    }
}
