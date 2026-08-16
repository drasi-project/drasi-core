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

//! Redb-backed `WalProvider` implementation.

use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::{CapacityPolicy, WalError, WalProvider, WriteAheadLogConfig};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::dto::{SourceChangeDto, WalRecord, WAL_FORMAT_VERSION};

/// Redb table for WAL events: sequence (u64) -> bincode-serialized `WalRecord`.
const EVENTS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("events");

/// Redb table for WAL metadata: string key -> byte value. Currently stores
/// only the monotonic counter under key `COUNTER_KEY`.
const METADATA_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

/// Key under which the last-assigned sequence number is persisted.
const COUNTER_KEY: &str = "counter";

/// Redb-backed Write-Ahead Log provider.
///
/// Manages one redb database file per registered source at
/// `{root_dir}/{source_id}.redb`. Per-source state (DB handle, in-memory counter,
/// config) is kept in an internal `HashMap` keyed by `source_id`.
///
/// WAL binary format is not stable across versions; WAL files should be
/// treated as ephemeral and are expected to be wiped on upgrade.
pub struct RedbWalProvider {
    root_dir: PathBuf,
    states: Arc<RwLock<HashMap<String, Arc<SourceWalState>>>>,
}

/// Per-source state managed internally by [`RedbWalProvider`].
struct SourceWalState {
    db: Arc<Database>,
    counter: AtomicU64,
    config: WriteAheadLogConfig,
    path: PathBuf,
}

impl RedbWalProvider {
    /// Create a provider rooted at `root_dir`. Each source gets a separate
    /// redb file at `{root_dir}/{source_id}.redb`.
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: root_dir.as_ref().to_path_buf(),
            states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn wal_path(&self, source_id: &str) -> PathBuf {
        self.root_dir.join(format!("{source_id}.redb"))
    }

    /// Validate that `source_id` is safe to use as a filename segment.
    ///
    /// Rejects empty strings, path separators (`/`, `\`), parent-directory
    /// tokens (`..`), hidden-file leading dots, NUL bytes, and any character
    /// outside the conservative set `[A-Za-z0-9_-]`. File-backed providers
    /// concatenate `source_id` into on-disk paths, so accepting arbitrary
    /// strings here would let callers write or delete files outside
    /// `root_dir` via a hostile or typo'd id.
    fn validate_source_id(source_id: &str) -> Result<(), WalError> {
        if source_id.is_empty() {
            return Err(WalError::InvalidSourceId(
                source_id.to_string(),
                "source_id must not be empty".to_string(),
            ));
        }
        if source_id == "." || source_id == ".." || source_id.starts_with('.') {
            return Err(WalError::InvalidSourceId(
                source_id.to_string(),
                "source_id must not start with '.'".to_string(),
            ));
        }
        for ch in source_id.chars() {
            if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
                return Err(WalError::InvalidSourceId(
                    source_id.to_string(),
                    format!(
                        "source_id may contain only ASCII alphanumerics, '_', or '-' (found '{ch}')"
                    ),
                ));
            }
        }
        Ok(())
    }

    async fn get_state(&self, source_id: &str) -> Result<Arc<SourceWalState>, WalError> {
        self.states
            .read()
            .await
            .get(source_id)
            .cloned()
            .ok_or_else(|| WalError::SourceNotRegistered(source_id.to_string()))
    }
}

#[async_trait]
impl WalProvider for RedbWalProvider {
    async fn register(&self, source_id: &str, config: WriteAheadLogConfig) -> Result<(), WalError> {
        // Enforces MIN_MAX_EVENTS
        config.validate()?;
        Self::validate_source_id(source_id)?;

        // Hold the write lock across the whole registration flow. Registration
        // is a cold, per-source startup path, so serializing it is fine, and
        // it lets us atomically gate the existence check, the DB open, and
        // the map insert. Releasing between the check and the insert would
        // let two concurrent callers race on the same source_id and clobber
        // each other's DB handle.
        let mut states = self.states.write().await;

        // Idempotency: re-registering with the same config is a no-op; a
        // different config on an already-registered source is an error.
        if let Some(existing) = states.get(source_id) {
            if existing.config.max_events == config.max_events
                && existing.config.capacity_policy == config.capacity_policy
            {
                return Ok(());
            } else {
                return Err(WalError::SourceAlreadyRegistered(source_id.to_string()));
            }
        }

        tokio::fs::create_dir_all(&self.root_dir)
            .await
            .map_err(|e| WalError::StorageError(format!("Failed to create WAL root dir: {e}")))?;

        let path = self.wal_path(source_id);
        let (db, counter_value) = open_or_create_db(path.clone()).await?;

        let state = Arc::new(SourceWalState {
            db: Arc::new(db),
            counter: AtomicU64::new(counter_value),
            config,
            path,
        });

        states.insert(source_id.to_string(), state);

        log::info!("WAL registered for source '{source_id}' (counter={counter_value})");
        Ok(())
    }

    async fn append(&self, source_id: &str, event: &SourceChange) -> Result<u64, WalError> {
        // Reject Future variant at the trait boundary
        if matches!(event, SourceChange::Future { .. }) {
            return Err(WalError::InvalidEvent(
                "SourceChange::Future events cannot be persisted to WAL".into(),
            ));
        }

        let state = self.get_state(source_id).await?;

        // Serialize via DTO before entering the blocking closure
        let dto: SourceChangeDto = event.into();
        let record = WalRecord {
            version: WAL_FORMAT_VERSION,
            change: dto,
        };
        let bytes = bincode::serialize(&record)
            .map_err(|e| WalError::SerializationError(format!("bincode serialize failed: {e}")))?;

        let source_id_owned = source_id.to_string();

        tokio::task::spawn_blocking(move || {
            let write_txn = state
                .db
                .begin_write()
                .map_err(|e| WalError::StorageError(format!("begin_write failed: {e}")))?;

            // Capacity check runs before sequence allocation. A rejected
            // append must not consume a sequence number, or we would leave
            // permanent gaps in the in-memory counter that outlive the
            // failed call. Allocating inside this block guarantees
            // fetch_add only runs on a path that will insert and commit.
            let sequence = {
                let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    WalError::StorageError(format!("open events table failed: {e}"))
                })?;

                let current_count = events_table
                    .len()
                    .map_err(|e| WalError::StorageError(format!("read event count failed: {e}")))?;

                if current_count >= state.config.max_events {
                    match state.config.capacity_policy {
                        CapacityPolicy::RejectIncoming => {
                            return Err(WalError::CapacityExhausted(source_id_owned));
                        }
                        CapacityPolicy::OverwriteOldest => {
                            // Loop until under capacity — handles the edge case
                            // where max_events was reduced across restarts,
                            // leaving the WAL over-capacity (inline review #7).
                            loop {
                                let current = events_table.len().map_err(|e| {
                                    WalError::StorageError(format!("read event count failed: {e}"))
                                })?;
                                if current < state.config.max_events {
                                    break;
                                }
                                // table.first() is O(log n) and makes intent
                                // explicit (inline review #6).
                                let oldest_key = events_table
                                    .first()
                                    .map_err(|e| {
                                        WalError::StorageError(format!(
                                            "read oldest entry failed: {e}"
                                        ))
                                    })?
                                    .map(|entry| entry.0.value());
                                match oldest_key {
                                    Some(key) => {
                                        events_table.remove(key).map_err(|e| {
                                            WalError::StorageError(format!(
                                                "evict oldest failed: {e}"
                                            ))
                                        })?;
                                    }
                                    None => break,
                                }
                            }
                        }
                    }
                }

                let sequence = state.counter.fetch_add(1, Ordering::SeqCst) + 1;

                events_table
                    .insert(sequence, bytes.as_slice())
                    .map_err(|e| WalError::StorageError(format!("insert event failed: {e}")))?;

                sequence
            };

            // Persist the counter in the same write transaction so recovery
            // is atomic with the event insert.
            {
                let mut metadata = write_txn.open_table(METADATA_TABLE).map_err(|e| {
                    WalError::StorageError(format!("open metadata table failed: {e}"))
                })?;
                metadata
                    .insert(COUNTER_KEY, sequence.to_le_bytes().as_slice())
                    .map_err(|e| WalError::StorageError(format!("update counter failed: {e}")))?;
            }

            write_txn
                .commit()
                .map_err(|e| WalError::StorageError(format!("commit failed: {e}")))?;

            Ok(sequence)
        })
        .await
        .map_err(|e| WalError::StorageError(format!("spawn_blocking join error: {e}")))?
    }

    async fn read_from(
        &self,
        source_id: &str,
        sequence: u64,
    ) -> Result<Vec<(u64, SourceChange)>, WalError> {
        let state = self.get_state(source_id).await?;
        let db = state.db.clone();
        let source_id_owned = source_id.to_string();

        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().map_err(|e| {
                WalError::StorageError(format!("begin_read failed: {e}"))
            })?;

            let table = match read_txn.open_table(EVENTS_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(e) => {
                    return Err(WalError::StorageError(format!(
                        "open events table failed: {e}"
                    )));
                }
            };

            let len = table
                .len()
                .map_err(|e| WalError::StorageError(format!("read event count failed: {e}")))?;
            if len > 0 {
                let first = table.first().map_err(|e| {
                    WalError::StorageError(format!("read first entry failed: {e}"))
                })?;
                if let Some(entry) = first {
                    let oldest = entry.0.value();
                    if sequence < oldest {
                        return Err(WalError::PositionUnavailable {
                            source_id: source_id_owned,
                            requested: sequence,
                            oldest_available: Some(oldest),
                        });
                    }
                }
            }

            let range = table.range(sequence..).map_err(|e| {
                WalError::StorageError(format!("create range iterator failed: {e}"))
            })?;

            let mut results = Vec::new();
            for entry in range {
                let entry = entry.map_err(|e| {
                    WalError::StorageError(format!("read entry failed: {e}"))
                })?;
                let seq = entry.0.value();
                let record: WalRecord = bincode::deserialize(entry.1.value()).map_err(|e| {
                    WalError::SerializationError(format!(
                        "bincode deserialize failed for seq {seq}: {e}"
                    ))
                })?;
                if record.version != WAL_FORMAT_VERSION {
                    return Err(WalError::SerializationError(format!(
                        "WAL format version mismatch for seq {seq}: expected {WAL_FORMAT_VERSION}, got {}",
                        record.version
                    )));
                }
                results.push((seq, record.change.into()));
            }

            Ok(results)
        })
        .await
        .map_err(|e| WalError::StorageError(format!("spawn_blocking join error: {e}")))?
    }

    async fn prune_up_to(&self, source_id: &str, sequence: u64) -> Result<u64, WalError> {
        let state = self.get_state(source_id).await?;
        let db = state.db.clone();

        tokio::task::spawn_blocking(move || {
            let write_txn = db
                .begin_write()
                .map_err(|e| WalError::StorageError(format!("begin_write failed: {e}")))?;

            let pruned_count;
            {
                let mut events_table = write_txn.open_table(EVENTS_TABLE).map_err(|e| {
                    WalError::StorageError(format!("open events table failed: {e}"))
                })?;

                // Collect keys first — redb borrows the table during iteration
                // so we can't mutate mid-iteration.
                let keys_to_remove: Vec<u64> = {
                    let range = events_table.range(..=sequence).map_err(|e| {
                        WalError::StorageError(format!("create range iterator failed: {e}"))
                    })?;
                    let mut keys = Vec::new();
                    for entry in range {
                        let entry = entry.map_err(|e| {
                            WalError::StorageError(format!("read entry failed: {e}"))
                        })?;
                        keys.push(entry.0.value());
                    }
                    keys
                };

                pruned_count = keys_to_remove.len() as u64;

                for key in keys_to_remove {
                    events_table.remove(key).map_err(|e| {
                        WalError::StorageError(format!("remove event {key} failed: {e}"))
                    })?;
                }
            }

            write_txn
                .commit()
                .map_err(|e| WalError::StorageError(format!("commit prune failed: {e}")))?;

            Ok(pruned_count)
        })
        .await
        .map_err(|e| WalError::StorageError(format!("spawn_blocking join error: {e}")))?
    }

    async fn head_sequence(&self, source_id: &str) -> Result<u64, WalError> {
        let state = self.get_state(source_id).await?;
        Ok(state.counter.load(Ordering::SeqCst))
    }

    async fn oldest_sequence(&self, source_id: &str) -> Result<Option<u64>, WalError> {
        let state = self.get_state(source_id).await?;
        let db = state.db.clone();

        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| WalError::StorageError(format!("begin_read failed: {e}")))?;

            let table = match read_txn.open_table(EVENTS_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(e) => {
                    return Err(WalError::StorageError(format!(
                        "open events table failed: {e}"
                    )));
                }
            };

            let first = table
                .first()
                .map_err(|e| WalError::StorageError(format!("read first failed: {e}")))?;
            Ok(first.map(|entry| entry.0.value()))
        })
        .await
        .map_err(|e| WalError::StorageError(format!("spawn_blocking join error: {e}")))?
    }

    async fn event_count(&self, source_id: &str) -> Result<u64, WalError> {
        let state = self.get_state(source_id).await?;
        let db = state.db.clone();

        tokio::task::spawn_blocking(move || {
            let read_txn = db
                .begin_read()
                .map_err(|e| WalError::StorageError(format!("begin_read failed: {e}")))?;

            let table = match read_txn.open_table(EVENTS_TABLE) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(e) => {
                    return Err(WalError::StorageError(format!(
                        "open events table failed: {e}"
                    )));
                }
            };

            table
                .len()
                .map_err(|e| WalError::StorageError(format!("read event count failed: {e}")))
        })
        .await
        .map_err(|e| WalError::StorageError(format!("spawn_blocking join error: {e}")))?
    }

    async fn delete_wal(&self, source_id: &str) -> Result<(), WalError> {
        Self::validate_source_id(source_id)?;

        // Remove from internal map first (drops Arc<SourceWalState>, which
        // drops the inner Arc<Database> once no other refs remain).
        let removed = self.states.write().await.remove(source_id);

        // Best-effort file deletion — whether or not the source was registered.
        let path = match &removed {
            Some(state) => state.path.clone(),
            None => self.wal_path(source_id),
        };

        if path.exists() {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| WalError::StorageError(format!("Failed to delete WAL file: {e}")))?;
        }
        Ok(())
    }
}

/// Open or create the redb database at `path` and restore the persisted
/// counter. Runs the blocking redb calls inside `spawn_blocking`.
async fn open_or_create_db(path: PathBuf) -> Result<(Database, u64), WalError> {
    tokio::task::spawn_blocking(move || {
        let db = Database::create(&path).map_err(|e| {
            WalError::StorageError(format!("Failed to open WAL database at {path:?}: {e}"))
        })?;

        let counter_value = {
            let read_txn = db
                .begin_read()
                .map_err(|e| WalError::StorageError(format!("begin_read failed: {e}")))?;

            match read_txn.open_table(METADATA_TABLE) {
                Ok(table) => match table.get(COUNTER_KEY) {
                    Ok(Some(value)) => {
                        let bytes = value.value();
                        // Persisted counter must be exactly 8 bytes (little-endian u64).
                        // A different length means the metadata table is corrupt —
                        // silently zeroing would let the next append reuse sequence
                        // numbers that already exist in the events table.
                        let arr: [u8; 8] = bytes.try_into().map_err(|_| {
                            WalError::StorageError(format!(
                                "WAL counter metadata corrupt for {path:?}: expected 8 bytes, got {}",
                                bytes.len()
                            ))
                        })?;
                        u64::from_le_bytes(arr)
                    }
                    Ok(None) => 0,
                    Err(e) => {
                        return Err(WalError::StorageError(format!("read counter failed: {e}")));
                    }
                },
                Err(redb::TableError::TableDoesNotExist(_)) => 0,
                Err(e) => {
                    return Err(WalError::StorageError(format!(
                        "open metadata table failed: {e}"
                    )));
                }
            }
        };

        Ok((db, counter_value))
    })
    .await
    .map_err(|e| WalError::StorageError(format!("spawn_blocking join error: {e}")))?
}
