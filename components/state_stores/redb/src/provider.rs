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
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

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
/// # Memory Considerations
///
/// This implementation uses `Box::leak` to create static string references required
/// by redb's `TableDefinition` API. Each unique `store_id` allocates a small amount
/// of memory (the table name string) that is never freed during the process lifetime.
///
/// **Important Limitation**: If your application creates many unique `store_id` values
/// (e.g., thousands of per-user or per-session stores), memory usage will grow
/// unboundedly. For typical use cases with a bounded number of plugins/stores,
/// this is negligible. The leaked memory is reclaimed when the process exits.
///
/// For applications that need to reclaim memory, use [`clear_table_name_cache()`]
/// to clear the cache (note: this doesn't reclaim already-leaked strings, but
/// prevents new allocations for the same store_ids).
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
    /// Cache of leaked table name references to avoid repeated allocations
    /// for the same store_id
    table_name_cache: Arc<RwLock<HashMap<String, &'static str>>>,
}

impl fmt::Debug for RedbStateStoreProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cache_len = self
            .table_name_cache
            .try_read()
            .map(|c| c.len())
            .unwrap_or(0);
        f.debug_struct("RedbStateStoreProvider")
            .field("cached_table_names", &cache_len)
            .finish_non_exhaustive()
    }
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
            StateStoreError::StorageError(format!(
                "Failed to create/open redb database at {:?}: {e}",
                path.as_ref()
            ))
        })?;

        info!("Opened redb state store at {:?}", path.as_ref());

        Ok(Self {
            db: Arc::new(db),
            table_name_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Clear the table name cache.
    ///
    /// This is useful for applications that create many unique `store_id` values
    /// and want to reduce memory growth. Note that this doesn't reclaim
    /// already-leaked strings, but it does prevent the cache from growing
    /// and ensures new accesses will reuse existing leaked strings if available.
    ///
    /// # Warning
    ///
    /// Calling this while operations are in progress may cause those operations
    /// to leak new table name strings. Only call this during idle periods.
    pub async fn clear_table_name_cache(&self) {
        let mut cache = self.table_name_cache.write().await;
        cache.clear();
    }

    /// Get the number of cached table names.
    ///
    /// This is useful for monitoring memory usage from leaked table name strings.
    pub async fn cached_table_name_count(&self) -> usize {
        self.table_name_cache.read().await.len()
    }

    /// Get the table name for a store_id.
    /// Table names are prefixed with "store_" to avoid conflicts.
    fn table_name(store_id: &str) -> String {
        format!("store_{store_id}")
    }

    /// Create a table definition from a static table name.
    #[inline]
    fn make_table_def(name: &'static str) -> TableDefinition<'static, &'static str, &'static [u8]> {
        TableDefinition::new(name)
    }

    /// Get or create a static table name reference for a store_id.
    /// This caches leaked strings to avoid allocating the same string multiple times.
    async fn get_or_create_table_name(&self, store_id: &str) -> &'static str {
        let table_name = Self::table_name(store_id);

        // Fast path: read lock
        if let Some(&name) = self.table_name_cache.read().await.get(&table_name) {
            return name;
        }

        // Slow path: write lock with entry API
        let mut cache = self.table_name_cache.write().await;
        *cache
            .entry(table_name.clone())
            .or_insert_with(|| Box::leak(table_name.into_boxed_str()))
    }
}

#[async_trait]
impl StateStoreProvider for RedbStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let key = key.to_string();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin read transaction for store '{store_id}': {e}"
                ))
            })?;

            let table_def = Self::make_table_def(table_name);

            match read_txn.open_table(table_def) {
                Ok(table) => {
                    let result = table.get(key.as_str()).map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to get key '{key}' from store '{store_id}': {e}"
                        ))
                    })?;

                    Ok(result.map(|v| v.value().to_vec()))
                }
                Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
                Err(e) => Err(StateStoreError::StorageError(format!(
                    "Failed to open table for store '{store_id}': {e}"
                ))),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let key = key.to_string();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin write transaction for store '{store_id}': {e}"
                ))
            })?;

            let result = {
                let table_def = Self::make_table_def(table_name);

                let mut table = write_txn.open_table(table_def).map_err(|e| {
                    StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    ))
                })?;

                table.insert(key.as_str(), value.as_slice()).map_err(|e| {
                    StateStoreError::StorageError(format!(
                        "Failed to insert key '{key}' into store '{store_id}': {e}"
                    ))
                })?;

                Ok::<_, StateStoreError>(())
            };

            match result {
                Ok(()) => {
                    write_txn.commit().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to commit transaction for store '{store_id}': {e}"
                        ))
                    })?;
                    debug!("Set key '{key}' in store '{store_id}'");
                    Ok(())
                }
                Err(e) => {
                    // Transaction will be aborted on drop
                    Err(e)
                }
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let key = key.to_string();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin write transaction for store '{store_id}': {e}"
                ))
            })?;

            let result = {
                let table_def = Self::make_table_def(table_name);

                match write_txn.open_table(table_def) {
                    Ok(mut table) => {
                        let removed = table.remove(key.as_str()).map_err(|e| {
                            StateStoreError::StorageError(format!(
                                "Failed to remove key '{key}' from store '{store_id}': {e}"
                            ))
                        })?;
                        Ok(removed.is_some())
                    }
                    Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
                    Err(e) => Err(StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    ))),
                }
            };

            match result {
                Ok(existed) => {
                    write_txn.commit().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to commit transaction for store '{store_id}': {e}"
                        ))
                    })?;
                    if existed {
                        debug!("Deleted key '{key}' from store '{store_id}'");
                    }
                    Ok(existed)
                }
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let key = key.to_string();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin read transaction for store '{store_id}': {e}"
                ))
            })?;

            let table_def = Self::make_table_def(table_name);

            match read_txn.open_table(table_def) {
                Ok(table) => {
                    let result = table.get(key.as_str()).map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to check key '{key}' in store '{store_id}': {e}"
                        ))
                    })?;
                    Ok(result.is_some())
                }
                Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
                Err(e) => Err(StateStoreError::StorageError(format!(
                    "Failed to open table for store '{store_id}': {e}"
                ))),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin read transaction for store '{store_id}': {e}"
                ))
            })?;

            let table_def = Self::make_table_def(table_name);
            let mut result = HashMap::new();

            match read_txn.open_table(table_def) {
                Ok(table) => {
                    for key in keys {
                        if let Some(value) = table.get(key.as_str()).map_err(|e| {
                            StateStoreError::StorageError(format!(
                                "Failed to get key '{key}' from store '{store_id}': {e}"
                            ))
                        })? {
                            result.insert(key, value.value().to_vec());
                        }
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    // Table doesn't exist, return empty result
                }
                Err(e) => {
                    return Err(StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    )))
                }
            }

            Ok(result)
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let store_id = store_id.to_string();
        let entry_count = entries.len();
        // Clone entries for the blocking closure
        let entries: Vec<(String, Vec<u8>)> = entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.to_vec()))
            .collect();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin write transaction for store '{store_id}': {e}"
                ))
            })?;

            let result = {
                let table_def = Self::make_table_def(table_name);

                let mut table = write_txn.open_table(table_def).map_err(|e| {
                    StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    ))
                })?;

                for (key, value) in &entries {
                    table.insert(key.as_str(), value.as_slice()).map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to insert key '{key}' into store '{store_id}': {e}"
                        ))
                    })?;
                }

                Ok::<_, StateStoreError>(())
            };

            match result {
                Ok(()) => {
                    write_txn.commit().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to commit transaction for store '{store_id}': {e}"
                        ))
                    })?;
                    debug!("Set {entry_count} entries in store '{store_id}'");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin write transaction for store '{store_id}': {e}"
                ))
            })?;

            let result = {
                let table_def = Self::make_table_def(table_name);

                match write_txn.open_table(table_def) {
                    Ok(mut table) => {
                        let mut count = 0;
                        for key in &keys {
                            let removed = table.remove(key.as_str()).map_err(|e| {
                                StateStoreError::StorageError(format!(
                                    "Failed to remove key '{key}' from store '{store_id}': {e}"
                                ))
                            })?;
                            if removed.is_some() {
                                count += 1;
                            }
                        }
                        Ok(count)
                    }
                    Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
                    Err(e) => Err(StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    ))),
                }
            };

            match result {
                Ok(count) => {
                    write_txn.commit().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to commit transaction for store '{store_id}': {e}"
                        ))
                    })?;
                    debug!("Deleted {count} keys from store '{store_id}'");
                    Ok(count)
                }
                Err(e) => Err(e),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin write transaction for store '{store_id}': {e}"
                ))
            })?;

            // Get count and delete table in one pass
            let table_def = Self::make_table_def(table_name);

            let count = match write_txn.open_table(table_def) {
                Ok(table) => {
                    let count = table.len().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to count entries in store '{store_id}': {e}"
                        ))
                    })? as usize;
                    // Need to drop table before we can delete it
                    drop(table);
                    count
                }
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(e) => {
                    return Err(StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    )))
                }
            };

            // Delete the table
            if count > 0 {
                let table_def = Self::make_table_def(table_name);
                write_txn.delete_table(table_def).map_err(|e| {
                    StateStoreError::StorageError(format!(
                        "Failed to delete table for store '{store_id}': {e}"
                    ))
                })?;
            }

            write_txn.commit().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to commit transaction for store '{store_id}': {e}"
                ))
            })?;

            debug!("Cleared store '{store_id}' ({count} entries)");
            Ok(count)
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin read transaction for store '{store_id}': {e}"
                ))
            })?;

            let table_def = Self::make_table_def(table_name);
            let mut keys = Vec::new();

            match read_txn.open_table(table_def) {
                Ok(table) => {
                    let iter = table.iter().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to iterate table for store '{store_id}': {e}"
                        ))
                    })?;

                    for entry in iter {
                        let (key, _) = entry.map_err(|e| {
                            StateStoreError::StorageError(format!(
                                "Failed to read entry from store '{store_id}': {e}"
                            ))
                        })?;
                        keys.push(key.value().to_string());
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    // Table doesn't exist, return empty list
                }
                Err(e) => {
                    return Err(StateStoreError::StorageError(format!(
                        "Failed to open table for store '{store_id}': {e}"
                    )))
                }
            }

            Ok(keys)
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin read transaction for store '{store_id}': {e}"
                ))
            })?;

            let table_def = Self::make_table_def(table_name);

            match read_txn.open_table(table_def) {
                Ok(table) => {
                    let len = table.len().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to get length of store '{store_id}': {e}"
                        ))
                    })?;
                    Ok(len > 0)
                }
                Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
                Err(e) => Err(StateStoreError::StorageError(format!(
                    "Failed to open table for store '{store_id}': {e}"
                ))),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        let table_name = self.get_or_create_table_name(store_id).await;
        let db = self.db.clone();
        let store_id = store_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                StateStoreError::StorageError(format!(
                    "Failed to begin read transaction for store '{store_id}': {e}"
                ))
            })?;

            let table_def = Self::make_table_def(table_name);

            match read_txn.open_table(table_def) {
                Ok(table) => {
                    let len = table.len().map_err(|e| {
                        StateStoreError::StorageError(format!(
                            "Failed to get length of store '{store_id}': {e}"
                        ))
                    })?;
                    Ok(len as usize)
                }
                Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
                Err(e) => Err(StateStoreError::StorageError(format!(
                    "Failed to open table for store '{store_id}': {e}"
                ))),
            }
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
    }

    async fn sync(&self) -> StateStoreResult<()> {
        // redb commits are synchronous and durable by default,
        // so sync is effectively a no-op. However, we can do a
        // checkpoint operation for consistency.
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || {
            // Perform a write transaction with no changes to force a sync
            let write_txn = db.begin_write().map_err(|e| {
                StateStoreError::StorageError(format!("Failed to begin sync transaction: {e}"))
            })?;

            write_txn.commit().map_err(|e| {
                StateStoreError::StorageError(format!("Failed to commit sync transaction: {e}"))
            })?;

            debug!("Synced redb state store");
            Ok(())
        })
        .await
        .map_err(|e| StateStoreError::Other(format!("Task join error: {e}")))?
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
    async fn test_redb_state_store_contains_key() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

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

        // Delete the key
        provider.delete("store1", "key1").await.unwrap();
        assert!(!provider.contains_key("store1", "key1").await.unwrap());
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
        assert!(!provider.store_exists("store1").await.unwrap());

        // Clear non-existent store should return 0
        let count = provider.clear_store("nonexistent").await.unwrap();
        assert_eq!(count, 0);
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
        assert!(!provider.store_exists("store1").await.unwrap());

        // Set a value
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert!(provider.store_exists("store1").await.unwrap());

        // Clear the store
        provider.clear_store("store1").await.unwrap();
        assert!(!provider.store_exists("store1").await.unwrap());
    }

    #[tokio::test]
    async fn test_redb_state_store_key_count() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Empty/non-existent store has 0 keys
        assert_eq!(provider.key_count("store1").await.unwrap(), 0);
        assert_eq!(provider.key_count("nonexistent").await.unwrap(), 0);

        // Set values
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);

        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 2);

        // Different store still has 0 keys
        assert_eq!(provider.key_count("store2").await.unwrap(), 0);

        // Delete a key
        provider.delete("store1", "key1").await.unwrap();
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);
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

        // Get multiple (including non-existent key)
        let result = provider
            .get_many("store1", &["key1", "key2", "nonexistent"])
            .await
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("key1"), Some(&b"value1".to_vec()));
        assert_eq!(result.get("key2"), Some(&b"value2".to_vec()));

        // Get from non-existent store returns empty HashMap
        let result = provider
            .get_many("nonexistent_store", &["key1", "key2"])
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_redb_state_store_set_many() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Set multiple values atomically
        provider
            .set_many("store1", &[("key1", b"value1"), ("key2", b"value2")])
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
                    ("key1", b"value1"),
                    ("key2", b"value2"),
                    ("key3", b"value3"),
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

    #[tokio::test]
    async fn test_redb_state_store_sync() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // sync should succeed
        provider.sync().await.unwrap();

        // Set some data and sync again
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider.sync().await.unwrap();

        // Verify data is still there
        let result = provider.get("store1", "key1").await.unwrap();
        assert_eq!(result, Some(b"value1".to_vec()));
    }

    #[tokio::test]
    async fn test_redb_state_store_debug_impl() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Debug should work
        let debug_str = format!("{provider:?}");
        assert!(debug_str.contains("RedbStateStoreProvider"));
        assert!(debug_str.contains("cached_table_names"));
    }

    #[tokio::test]
    async fn test_redb_state_store_table_name_cache() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Initially no cached names
        assert_eq!(provider.cached_table_name_count().await, 0);

        // Access a store
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert_eq!(provider.cached_table_name_count().await, 1);

        // Access another store
        provider
            .set("store2", "key1", b"value1".to_vec())
            .await
            .unwrap();
        assert_eq!(provider.cached_table_name_count().await, 2);

        // Clear the cache
        provider.clear_table_name_cache().await;
        assert_eq!(provider.cached_table_name_count().await, 0);
    }

    #[tokio::test]
    async fn test_redb_concurrent_access() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = Arc::new(RedbStateStoreProvider::new(&db_path).unwrap());

        // Spawn multiple concurrent tasks
        let mut handles = vec![];
        for i in 0..10 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move {
                let key = format!("key{i}");
                let value = vec![i as u8; 10];
                p.set("store", &key, value.clone()).await.unwrap();
                let result = p.get("store", &key).await.unwrap();
                assert_eq!(result, Some(value));
            }));
        }

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all keys exist
        assert_eq!(provider.key_count("store").await.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_redb_concurrent_different_stores() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = Arc::new(RedbStateStoreProvider::new(&db_path).unwrap());

        // Spawn tasks that write to different stores concurrently
        let mut handles = vec![];
        for i in 0..10 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move {
                let store_id = format!("store{i}");
                for j in 0..5 {
                    let key = format!("key{j}");
                    let value = vec![i as u8, j as u8];
                    p.set(&store_id, &key, value).await.unwrap();
                }
            }));
        }

        // Wait for all to complete
        for handle in handles {
            handle.await.unwrap();
        }

        // Verify each store has 5 keys
        for i in 0..10 {
            let store_id = format!("store{i}");
            assert_eq!(provider.key_count(&store_id).await.unwrap(), 5);
        }
    }

    // =========================================================================
    // Edge Case Tests
    // =========================================================================

    #[tokio::test]
    async fn test_redb_empty_key() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Empty key should work
        provider.set("store1", "", b"value".to_vec()).await.unwrap();
        let result = provider.get("store1", "").await.unwrap();
        assert_eq!(result, Some(b"value".to_vec()));

        // Should be listable
        let keys = provider.list_keys("store1").await.unwrap();
        assert_eq!(keys, vec![""]);
    }

    #[tokio::test]
    async fn test_redb_empty_value() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Empty value should work
        provider.set("store1", "key", vec![]).await.unwrap();
        let result = provider.get("store1", "key").await.unwrap();
        assert_eq!(result, Some(vec![]));

        // Key should exist and be counted
        assert!(provider.contains_key("store1", "key").await.unwrap());
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_redb_empty_store_id() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Empty store_id should work (prefixed with "store_")
        provider.set("", "key", b"value".to_vec()).await.unwrap();
        let result = provider.get("", "key").await.unwrap();
        assert_eq!(result, Some(b"value".to_vec()));
    }

    #[tokio::test]
    async fn test_redb_unicode_keys() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Unicode in keys
        provider
            .set("store1", "キー", b"value".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "مفتاح", b"value2".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "🔑", b"value3".to_vec())
            .await
            .unwrap();

        assert_eq!(
            provider.get("store1", "キー").await.unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(
            provider.get("store1", "مفتاح").await.unwrap(),
            Some(b"value2".to_vec())
        );
        assert_eq!(
            provider.get("store1", "🔑").await.unwrap(),
            Some(b"value3".to_vec())
        );

        assert_eq!(provider.key_count("store1").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn test_redb_unicode_store_id() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Unicode in store_id
        provider
            .set("商店", "key", b"value".to_vec())
            .await
            .unwrap();
        provider
            .set("متجر", "key", b"value2".to_vec())
            .await
            .unwrap();

        assert_eq!(
            provider.get("商店", "key").await.unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(
            provider.get("متجر", "key").await.unwrap(),
            Some(b"value2".to_vec())
        );
    }

    #[tokio::test]
    async fn test_redb_special_characters() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Special characters in keys
        let special_keys = [
            "key:with:colons",
            "key/with/slashes",
            "key\\with\\backslashes",
            "key\twith\ttabs",
            "key\nwith\nnewlines",
            "key with spaces",
            "key\0with\0nulls",
            "key\"with\"quotes",
            "key'with'apostrophes",
        ];

        for (i, key) in special_keys.iter().enumerate() {
            provider.set("store1", key, vec![i as u8]).await.unwrap();
        }

        for (i, key) in special_keys.iter().enumerate() {
            let result = provider.get("store1", key).await.unwrap();
            assert_eq!(result, Some(vec![i as u8]), "Failed for key: {key:?}");
        }

        assert_eq!(
            provider.key_count("store1").await.unwrap(),
            special_keys.len()
        );
    }

    #[tokio::test]
    async fn test_redb_large_value() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Large value (1MB)
        let large_value: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
        provider
            .set("store1", "large", large_value.clone())
            .await
            .unwrap();

        let result = provider.get("store1", "large").await.unwrap();
        assert_eq!(result, Some(large_value));
    }

    #[tokio::test]
    async fn test_redb_long_key() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Very long key (10KB)
        let long_key: String = "k".repeat(10_000);
        provider
            .set("store1", &long_key, b"value".to_vec())
            .await
            .unwrap();

        let result = provider.get("store1", &long_key).await.unwrap();
        assert_eq!(result, Some(b"value".to_vec()));
    }

    #[tokio::test]
    async fn test_redb_long_store_id() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Very long store_id (1KB - keeping shorter for table names)
        let long_store_id: String = "s".repeat(1_000);
        provider
            .set(&long_store_id, "key", b"value".to_vec())
            .await
            .unwrap();

        let result = provider.get(&long_store_id, "key").await.unwrap();
        assert_eq!(result, Some(b"value".to_vec()));
    }

    #[tokio::test]
    async fn test_redb_overwrite() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Overwrite existing key
        provider
            .set("store1", "key", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key", b"value2".to_vec())
            .await
            .unwrap();

        let result = provider.get("store1", "key").await.unwrap();
        assert_eq!(result, Some(b"value2".to_vec()));

        // Should still be just 1 key
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_redb_get_many_empty_keys() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();

        // Empty keys array
        let result = provider.get_many("store1", &[]).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_redb_set_many_empty() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Empty entries should succeed
        provider.set_many("store1", &[]).await.unwrap();

        // Store should not exist (no data added)
        assert!(!provider.store_exists("store1").await.unwrap());
    }

    #[tokio::test]
    async fn test_redb_delete_many_empty() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();

        // Empty keys array should delete nothing
        let count = provider.delete_many("store1", &[]).await.unwrap();
        assert_eq!(count, 0);

        // Key should still exist
        assert!(provider.contains_key("store1", "key1").await.unwrap());

        // Delete from non-existent store should return 0
        let count = provider
            .delete_many("nonexistent_store", &["key1", "key2"])
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_redb_persistence_across_instances() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // First instance - write data
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();
            provider
                .set_many(
                    "store1",
                    &[
                        ("key1", b"value1"),
                        ("key2", b"value2"),
                        ("key3", b"value3"),
                    ],
                )
                .await
                .unwrap();
        }

        // Second instance - verify data persisted
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();

            assert!(provider.store_exists("store1").await.unwrap());
            assert_eq!(provider.key_count("store1").await.unwrap(), 3);

            let result = provider
                .get_many("store1", &["key1", "key2", "key3"])
                .await
                .unwrap();
            assert_eq!(result.len(), 3);
            assert_eq!(result.get("key1"), Some(&b"value1".to_vec()));
            assert_eq!(result.get("key2"), Some(&b"value2".to_vec()));
            assert_eq!(result.get("key3"), Some(&b"value3".to_vec()));
        }

        // Third instance - modify and verify
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();

            // Delete one key
            provider.delete("store1", "key2").await.unwrap();

            // Update another
            provider
                .set("store1", "key1", b"updated".to_vec())
                .await
                .unwrap();

            assert_eq!(provider.key_count("store1").await.unwrap(), 2);
        }

        // Fourth instance - verify final state
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();

            assert_eq!(
                provider.get("store1", "key1").await.unwrap(),
                Some(b"updated".to_vec())
            );
            assert_eq!(provider.get("store1", "key2").await.unwrap(), None);
            assert_eq!(
                provider.get("store1", "key3").await.unwrap(),
                Some(b"value3".to_vec())
            );
        }
    }

    #[tokio::test]
    async fn test_redb_multiple_stores_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // Create multiple stores
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();

            for i in 0..5 {
                let store_id = format!("store{i}");
                for j in 0..3 {
                    let key = format!("key{j}");
                    let value = format!("value_{i}_{j}").into_bytes();
                    provider.set(&store_id, &key, value).await.unwrap();
                }
            }
        }

        // Verify all stores persisted
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();

            for i in 0..5 {
                let store_id = format!("store{i}");
                assert!(provider.store_exists(&store_id).await.unwrap());
                assert_eq!(provider.key_count(&store_id).await.unwrap(), 3);
            }
        }

        // Clear one store and verify others unaffected
        {
            let provider = RedbStateStoreProvider::new(&db_path).unwrap();

            provider.clear_store("store2").await.unwrap();

            assert!(!provider.store_exists("store2").await.unwrap());
            assert!(provider.store_exists("store0").await.unwrap());
            assert!(provider.store_exists("store4").await.unwrap());
        }
    }

    #[tokio::test]
    async fn test_redb_table_name_cache_reuse() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Access same store multiple times
        for i in 0..10 {
            provider
                .set("store1", &format!("key{i}"), vec![i as u8])
                .await
                .unwrap();
        }

        // Should only have one cached table name
        assert_eq!(provider.cached_table_name_count().await, 1);

        // Access different stores
        provider.set("store2", "key", vec![]).await.unwrap();
        provider.set("store3", "key", vec![]).await.unwrap();

        // Now should have 3 cached names
        assert_eq!(provider.cached_table_name_count().await, 3);
    }

    #[tokio::test]
    async fn test_redb_clear_and_reuse_store() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = RedbStateStoreProvider::new(&db_path).unwrap();

        // Add data
        provider
            .set("store1", "key1", b"value1".to_vec())
            .await
            .unwrap();
        provider
            .set("store1", "key2", b"value2".to_vec())
            .await
            .unwrap();

        assert_eq!(provider.key_count("store1").await.unwrap(), 2);

        // Clear the store
        let count = provider.clear_store("store1").await.unwrap();
        assert_eq!(count, 2);
        assert!(!provider.store_exists("store1").await.unwrap());

        // Reuse the same store
        provider
            .set("store1", "newkey", b"newvalue".to_vec())
            .await
            .unwrap();

        assert!(provider.store_exists("store1").await.unwrap());
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);
        assert_eq!(
            provider.get("store1", "newkey").await.unwrap(),
            Some(b"newvalue".to_vec())
        );
    }

    #[tokio::test]
    async fn test_redb_batch_operations_atomicity() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = Arc::new(RedbStateStoreProvider::new(&db_path).unwrap());

        // Set many should be atomic
        provider
            .set_many(
                "store1",
                &[
                    ("key1", b"value1"),
                    ("key2", b"value2"),
                    ("key3", b"value3"),
                ],
            )
            .await
            .unwrap();

        // All should exist
        assert_eq!(provider.key_count("store1").await.unwrap(), 3);

        // Delete many should be atomic
        let deleted = provider
            .delete_many("store1", &["key1", "key2"])
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(provider.key_count("store1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_redb_high_concurrency_same_store() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let provider = Arc::new(RedbStateStoreProvider::new(&db_path).unwrap());

        // Spawn many concurrent tasks writing to same store
        let mut handles = vec![];
        for i in 0..50 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move {
                let key = format!("key{i}");
                let value = vec![i as u8; 100];
                p.set("store", &key, value.clone()).await.unwrap();

                // Verify our write
                let result = p.get("store", &key).await.unwrap();
                assert_eq!(result, Some(value));
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // Verify all keys written
        assert_eq!(provider.key_count("store").await.unwrap(), 50);
    }
}
