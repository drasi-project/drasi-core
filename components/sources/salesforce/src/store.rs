use anyhow::{anyhow, ensure, Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::BootstrapResult;
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_salesforce_common::{
    graph_changes, sha256_hex, write_snapshot_record, DiscoveredSchema, GraphRecord, LocalPosition,
    SnapshotManifest, SnapshotTicket, MAX_SNAPSHOT_FILE_BYTES, SNAPSHOT_MANIFEST_VERSION,
};
use futures_util::FutureExt;
use redb::{
    backends::FileBackend, Database, ReadableTable, ReadableTableMetadata, StorageBackend,
    TableDefinition,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::task::spawn_blocking;
use uuid::Uuid;

const FORMAT_VERSION: u32 = 2;
const DB_FILE: &str = "mirror.redb";
const OWNERSHIP_FILE: &str = "ownership.json";
const SNAPSHOT_DIR: &str = "snapshots";

const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const ROWS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("rows");
const EVENTS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("events");
const LEASES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("snapshot_leases");

const KEY_FORMAT_VERSION: &str = "format_version";
const KEY_SOURCE_ID: &str = "source_id";
const KEY_SOURCE_IDENTITY: &str = "source_identity";
const KEY_EPOCH: &str = "epoch";
const KEY_HEAD: &str = "head_ordinal";
const KEY_SCHEMA: &str = "schema";
const KEY_SCHEMA_PENDING: &str = "schema_pending_records";

#[derive(Clone, Debug)]
pub struct StorageLimits {
    pub max_rows: usize,
    pub max_events: usize,
    pub retention_hours: u64,
    pub max_projected_row_bytes: usize,
    pub max_snapshot_leases: usize,
    pub max_spool_bytes: u64,
}

#[derive(Clone, Debug)]
pub enum RecordMutation {
    Upsert(GraphRecord),
    Delete {
        object: String,
        id: String,
        observed_at_ms: u64,
        schema_revision: String,
    },
}

#[derive(Debug)]
pub struct StaleSchema;

impl std::fmt::Display for StaleSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "Salesforce metadata changed during observation; refetch using the active schema",
        )
    }
}

impl std::error::Error for StaleSchema {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SalesforceSchema {
    pub source_id: String,
    pub source_identity: String,
    pub org_id: String,
    pub epoch: String,
    pub head_ordinal: u64,
    pub pending_records: u64,
    pub schema: DiscoveredSchema,
}

#[derive(Clone)]
pub struct DurableMirrorStore {
    source_id: String,
    source_identity: String,
    objects: Vec<String>,
    org_id: String,
    root_dir: PathBuf,
    snapshots_dir: PathBuf,
    limits: StorageLimits,
    db: Arc<Database>,
    spool_budget: Arc<SpoolBudget>,
    epoch: u128,
    replay_pins: Arc<StdMutex<HashMap<u64, Arc<RetentionBoundary>>>>,
    next_replay_pin_id: Arc<AtomicU64>,
    snapshot_export:
        Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<SnapshotTicket>>>>>,
    snapshot_readers: Arc<tokio::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

enum RetentionBoundary {
    Replay(u64),
    Subscriber {
        after: u64,
        confirmed: Weak<AtomicU64>,
    },
}

impl RetentionBoundary {
    fn first_required(&self) -> Option<u64> {
        match self {
            Self::Replay(first) => Some(*first),
            Self::Subscriber { after, confirmed } => {
                let sequence = confirmed.upgrade()?.load(Ordering::Acquire);
                let after = if sequence == u64::MAX {
                    *after
                } else {
                    (*after).max(sequence)
                };
                Some(after.saturating_add(1))
            }
        }
    }
}

pub struct ReplayRetentionPin {
    pin_id: u64,
    pins: Weak<StdMutex<HashMap<u64, Arc<RetentionBoundary>>>>,
    boundary: Arc<RetentionBoundary>,
}

impl ReplayRetentionPin {
    pub fn preserves_live_history(&self) -> bool {
        matches!(self.boundary.as_ref(), RetentionBoundary::Subscriber { .. })
    }
}

impl Drop for ReplayRetentionPin {
    fn drop(&mut self) {
        if let Some(pins) = self.pins.upgrade() {
            match pins.lock() {
                Ok(mut guard) => {
                    guard.remove(&self.pin_id);
                }
                Err(_) => log::error!("Salesforce retention pin registry poisoned during release"),
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredEvent {
    timestamp_ms: i64,
    source_position: Vec<u8>,
    change: SourceChange,
}

#[derive(Debug, Serialize, Deserialize)]
struct LeaseRecord {
    ticket_id: String,
    manifest_rel_path: String,
    expires_at_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OwnershipFile {
    format_version: u32,
    source_id: String,
    source_identity: String,
}

#[derive(Debug)]
struct SpoolBudget {
    root: PathBuf,
    max_bytes: u64,
    growth: StdMutex<()>,
}

impl SpoolBudget {
    fn with_file_tree<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self
            .growth
            .lock()
            .map_err(|_| anyhow!("Salesforce spool budget lock poisoned"))?;
        operation()
    }

    fn usage(&self) -> Result<u64> {
        self.with_file_tree(|| compute_spool_usage_bytes(&self.root))
    }

    fn remove_snapshot(&self, root: &Path, path: &Path) -> Result<()> {
        self.with_file_tree(|| safe_remove_owned_path(root, path))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        self.with_file_tree(|| Ok(std::fs::rename(from, to)?))
    }

    fn grow<T>(
        &self,
        current: impl FnOnce() -> io::Result<u64>,
        target: u64,
        write: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let _guard = self
            .growth
            .lock()
            .map_err(|_| io::Error::other("Salesforce spool budget lock poisoned"))?;
        let current = current()?;
        if target > current {
            let usage = compute_spool_usage_bytes(&self.root).map_err(io::Error::other)?;
            ensure_spool_budget(usage, target - current, self.max_bytes)
                .map_err(io::Error::other)?;
        }
        write()
    }

    fn create_file(self: &Arc<Self>, path: &Path) -> io::Result<BudgetedFile> {
        Ok(BudgetedFile {
            file: std::fs::File::create(path)?,
            budget: self.clone(),
        })
    }

    fn write_file(self: &Arc<Self>, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut file = self.create_file(path)?;
        file.write_all(bytes)?;
        file.file.sync_all()
    }
}

#[derive(Debug)]
struct BudgetedBackend {
    file: FileBackend,
    budget: Arc<SpoolBudget>,
}

impl StorageBackend for BudgetedBackend {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }

    fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.file.read(offset, len)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.budget
            .grow(|| self.file.len(), len, || self.file.set_len(len))
    }

    fn sync_data(&self, eventual: bool) -> io::Result<()> {
        self.file.sync_data(eventual)
    }

    fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("Salesforce database offset overflow"))?;
        self.budget
            .grow(|| self.file.len(), end, || self.file.write(offset, bytes))
    }
}

struct BudgetedFile {
    file: std::fs::File,
    budget: Arc<SpoolBudget>,
}

impl Write for BudgetedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .file
            .stream_position()?
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("Salesforce spool offset overflow"))?;
        let mut file = &self.file;
        self.budget.grow(
            || self.file.metadata().map(|meta| meta.len()),
            end,
            || file.write(bytes),
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl DurableMirrorStore {
    pub async fn open(
        root_dir: PathBuf,
        source_id: String,
        source_identity: String,
        org_id: String,
        objects: Vec<String>,
        limits: StorageLimits,
    ) -> Result<Self> {
        let snapshots_dir = root_dir.join(SNAPSHOT_DIR);
        let ownership_path = root_dir.join(OWNERSHIP_FILE);
        let db_path = root_dir.join(DB_FILE);
        let source_id_clone = source_id.clone();
        let source_identity_clone = source_identity.clone();
        let root_clone = root_dir.clone();
        let snapshots_clone = snapshots_dir.clone();
        let ownership_clone = ownership_path.clone();
        let db_path_clone = db_path.clone();
        let limits_clone = limits.clone();
        let spool_budget = Arc::new(SpoolBudget {
            root: root_dir.clone(),
            max_bytes: limits.max_spool_bytes,
            growth: StdMutex::new(()),
        });
        let budget = spool_budget.clone();

        let (db, epoch) = spawn_blocking(move || -> Result<(Database, u128)> {
            std::fs::create_dir_all(&root_clone)?;
            ensure!(
                ownership_clone.exists() || !db_path_clone.exists(),
                "refusing to adopt an unowned Salesforce database"
            );
            ensure_owned_root(
                &ownership_clone,
                &source_id_clone,
                &source_identity_clone,
                &budget,
            )?;
            std::fs::create_dir_all(&snapshots_clone)?;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&db_path_clone)?;
            let backend = BudgetedBackend {
                file: FileBackend::new(file)?,
                budget,
            };
            let db = Database::builder()
                .create_with_backend(backend)
                .with_context(|| format!("failed to open redb file {}", db_path_clone.display()))?;

            let write = db.begin_write()?;
            {
                let _rows = write.open_table(ROWS_TABLE)?;
                let _events = write.open_table(EVENTS_TABLE)?;
                let _leases = write.open_table(LEASES_TABLE)?;
                let mut meta = write.open_table(META_TABLE)?;
                let has_version = meta.get(KEY_FORMAT_VERSION)?.is_some();
                if !has_version {
                    let epoch = Uuid::new_v4().as_u128();
                    let format_bytes = FORMAT_VERSION.to_be_bytes();
                    meta.insert(KEY_FORMAT_VERSION, format_bytes.as_slice())?;
                    meta.insert(KEY_SOURCE_ID, source_id_clone.as_bytes())?;
                    meta.insert(KEY_SOURCE_IDENTITY, source_identity_clone.as_bytes())?;
                    let epoch_bytes = epoch.to_be_bytes();
                    meta.insert(KEY_EPOCH, epoch_bytes.as_slice())?;
                    let head_bytes = 0u64.to_be_bytes();
                    meta.insert(KEY_HEAD, head_bytes.as_slice())?;
                } else {
                    validate_meta(&meta, &source_id_clone, &source_identity_clone)?;
                }
            }
            write.commit()?;

            let read = db.begin_read()?;
            let meta = read.open_table(META_TABLE)?;
            let epoch = decode_u128(
                meta.get(KEY_EPOCH)?
                    .context("missing epoch metadata")?
                    .value(),
            )?;
            cleanup_orphan_snapshots(&db, &snapshots_clone)?;
            let spool_usage = compute_spool_usage_bytes(&root_clone)?;
            ensure!(
                spool_usage <= limits_clone.max_spool_bytes,
                "aggregate spool usage {} bytes exceeds max_spool_bytes limit ({})",
                spool_usage,
                limits_clone.max_spool_bytes
            );
            Ok((db, epoch))
        })
        .await??;

        Ok(Self {
            source_id,
            source_identity,
            objects,
            org_id,
            root_dir,
            snapshots_dir,
            limits,
            db: Arc::new(db),
            spool_budget,
            epoch,
            replay_pins: Arc::new(StdMutex::new(HashMap::new())),
            next_replay_pin_id: Arc::new(AtomicU64::new(1)),
            snapshot_export: Arc::new(tokio::sync::Mutex::new(None)),
            snapshot_readers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }

    pub fn epoch(&self) -> u128 {
        self.epoch
    }

    pub async fn head_ordinal(&self) -> Result<u64> {
        let db = self.db.clone();
        spawn_blocking(move || -> Result<u64> {
            let read = db.begin_read()?;
            let meta = read.open_table(META_TABLE)?;
            decode_u64(
                meta.get(KEY_HEAD)?
                    .context("missing head ordinal metadata")?
                    .value(),
            )
        })
        .await?
    }

    pub async fn earliest_resumable_ordinal(&self) -> Result<u64> {
        let db = self.db.clone();
        spawn_blocking(move || -> Result<u64> {
            let read = db.begin_read()?;
            let meta = read.open_table(META_TABLE)?;
            let head = decode_u64(
                meta.get(KEY_HEAD)?
                    .context("missing head ordinal metadata")?
                    .value(),
            )?;
            let events = read.open_table(EVENTS_TABLE)?;
            let earliest = if let Some(entry) = events.first()? {
                entry.0.value().saturating_sub(1)
            } else {
                head
            };
            Ok(earliest)
        })
        .await?
    }

    pub async fn record_ids(&self, object: &str) -> Result<Vec<String>> {
        let db = self.db.clone();
        let prefix = format!("{object}:");
        let end = format!("{object};");
        spawn_blocking(move || -> Result<Vec<String>> {
            let read = db.begin_read()?;
            let rows = read.open_table(ROWS_TABLE)?;
            rows.range(prefix.as_str()..end.as_str())?
                .map(|entry| {
                    let (key, _) = entry?;
                    Ok(key
                        .value()
                        .strip_prefix(&prefix)
                        .context("invalid record key")?
                        .to_owned())
                })
                .collect()
        })
        .await?
    }

    #[cfg(test)]
    pub async fn load_all_rows(&self) -> Result<Vec<GraphRecord>> {
        let db = self.db.clone();
        spawn_blocking(move || -> Result<Vec<GraphRecord>> {
            let read = db.begin_read()?;
            let rows = read.open_table(ROWS_TABLE)?;
            rows.iter()?
                .map(|entry| -> Result<GraphRecord> {
                    let (_, value) = entry?;
                    Ok(rmp_serde::from_slice(value.value())?)
                })
                .collect::<Result<Vec<_>>>()
        })
        .await?
    }

    pub async fn set_schema(&self, schema: DiscoveredSchema) -> Result<()> {
        let db = self.db.clone();
        let budget = self.spool_budget.clone();
        let max_spool_bytes = self.limits.max_spool_bytes;
        spawn_blocking(move || -> Result<()> {
            let bytes = serde_json::to_vec(&schema)?;
            ensure!(
                bytes.len() <= 16 * 1024 * 1024,
                "Salesforce schema exceeds 16 MiB"
            );
            ensure_spool_budget(budget.usage()?, bytes.len() as u64, max_spool_bytes)?;
            let write = db.begin_write()?;
            {
                let rows = write.open_table(ROWS_TABLE)?;
                let mut meta = write.open_table(META_TABLE)?;
                let mut pending = 0u64;
                for entry in rows.iter()? {
                    let (_, bytes) = entry?;
                    let record: GraphRecord = rmp_serde::from_slice(bytes.value())?;
                    pending += u64::from(record.schema_revision != schema.fingerprint);
                }
                meta.insert(KEY_SCHEMA, bytes.as_slice())?;
                meta.insert(KEY_SCHEMA_PENDING, pending.to_be_bytes().as_slice())?;
            }
            write.commit()?;
            Ok(())
        })
        .await?
    }

    pub async fn schema(&self) -> Result<SalesforceSchema> {
        let db = self.db.clone();
        let source_id = self.source_id.clone();
        let source_identity = self.source_identity.clone();
        let org_id = self.org_id.clone();
        let epoch = self.epoch.to_string();
        spawn_blocking(move || -> Result<SalesforceSchema> {
            let read = db.begin_read()?;
            let meta = read.open_table(META_TABLE)?;
            let schema = serde_json::from_slice(
                meta.get(KEY_SCHEMA)?
                    .context("Salesforce schema is not ready; start the source first")?
                    .value(),
            )?;
            Ok(SalesforceSchema {
                source_id,
                source_identity,
                org_id,
                epoch,
                schema,
                head_ordinal: decode_u64(
                    meta.get(KEY_HEAD)?.context("missing head ordinal")?.value(),
                )?,
                pending_records: decode_u64(
                    meta.get(KEY_SCHEMA_PENDING)?
                        .context("missing schema progress")?
                        .value(),
                )?,
            })
        })
        .await?
    }

    pub async fn publish_schema(&self) -> Result<()> {
        let report = self.schema().await?;
        let root = self.root_dir.clone();
        let max_bytes = self.limits.max_spool_bytes;
        let budget = self.spool_budget.clone();
        spawn_blocking(move || -> Result<()> {
            let bytes = serde_json::to_vec_pretty(&report)?;
            ensure_spool_budget(budget.usage()?, bytes.len() as u64, max_bytes)?;
            let temporary = root.join("schema.json.tmp");
            budget.write_file(&temporary, &bytes)?;
            budget.rename(&temporary, &root.join("schema.json"))?;
            std::fs::File::open(root)?.sync_all()?;
            Ok(())
        })
        .await?
    }

    pub async fn register_replay_pin(
        &self,
        start_ordinal_exclusive: u64,
        end_ordinal_inclusive: u64,
    ) -> Result<Option<ReplayRetentionPin>> {
        if start_ordinal_exclusive >= end_ordinal_inclusive {
            return Ok(None);
        }
        self.register_pin(RetentionBoundary::Replay(
            start_ordinal_exclusive.saturating_add(1),
        ))
        .map(Some)
    }

    pub fn register_subscription_pin(
        &self,
        after: u64,
        confirmed: &Arc<AtomicU64>,
    ) -> Result<ReplayRetentionPin> {
        self.register_pin(RetentionBoundary::Subscriber {
            after,
            confirmed: Arc::downgrade(confirmed),
        })
    }

    pub fn clear_retention_pins(&self) -> Result<()> {
        self.replay_pins
            .lock()
            .map_err(|_| anyhow!("Salesforce retention pin registry poisoned"))?
            .clear();
        Ok(())
    }

    fn register_pin(&self, boundary: RetentionBoundary) -> Result<ReplayRetentionPin> {
        let pin_id = self.next_replay_pin_id.fetch_add(1, Ordering::Relaxed);
        let boundary = Arc::new(boundary);
        self.replay_pins
            .lock()
            .map_err(|_| anyhow!("Salesforce retention pin registry poisoned"))?
            .insert(pin_id, boundary.clone());
        Ok(ReplayRetentionPin {
            pin_id,
            pins: Arc::downgrade(&self.replay_pins),
            boundary,
        })
    }

    pub async fn commit_mutations(
        &self,
        mutations: Vec<RecordMutation>,
    ) -> Result<Vec<Arc<SourceEventWrapper>>> {
        if mutations.is_empty() {
            return Ok(Vec::new());
        }

        let db = self.db.clone();
        let budget = self.spool_budget.clone();
        let source_id = self.source_id.clone();
        let org_id = self.org_id.clone();
        let objects = self.objects.clone();
        let epoch = self.epoch;
        let limits = self.limits.clone();
        let pins = self.replay_pins.clone();

        spawn_blocking(move || -> Result<Vec<Arc<SourceEventWrapper>>> {
            let current_spool_usage = budget.usage()?;
            let write = db.begin_write()?;
            let minimum_pinned_ordinal = pins
                .lock()
                .map_err(|_| anyhow!("Salesforce retention pin registry poisoned"))?
                .values()
                .filter_map(|boundary| boundary.first_required())
                .min();
            let wrappers = {
                let mut rows = write.open_table(ROWS_TABLE)?;
                let mut events = write.open_table(EVENTS_TABLE)?;
                let mut meta = write.open_table(META_TABLE)?;

                let mut head = decode_u64(
                    meta.get(KEY_HEAD)?
                        .context("missing head ordinal metadata")?
                        .value(),
                )?;
                let mut wrappers = Vec::with_capacity(mutations.len());
                let now = Utc::now().timestamp_millis();
                let timestamp = Utc::now();
                let mut projected_growth_bytes = 0u64;
                let schema: Option<DiscoveredSchema> = meta
                    .get(KEY_SCHEMA)?
                    .map(|bytes| serde_json::from_slice(bytes.value()))
                    .transpose()?;
                let mut pending = meta
                    .get(KEY_SCHEMA_PENDING)?
                    .map(|value| decode_u64(value.value()))
                    .transpose()?
                    .unwrap_or(0);

                for mutation in mutations {
                    let (object, id, mut next, deleted_at_ms, revision) = match mutation {
                        RecordMutation::Upsert(row) => {
                            let object = row.object.clone();
                            let id = row.id.clone();
                            let timestamp = row.observed_at_ms;
                            let revision = row.schema_revision.clone();
                            (object, id, Some(row), timestamp, revision)
                        }
                        RecordMutation::Delete {
                            object,
                            id,
                            observed_at_ms,
                            schema_revision,
                        } => (object, id, None, observed_at_ms, schema_revision),
                    };
                    ensure!(
                        objects.contains(&object),
                        "record belongs to unselected Salesforce object '{object}'"
                    );
                    if schema
                        .as_ref()
                        .is_some_and(|schema| schema.fingerprint != revision)
                    {
                        return Err(StaleSchema.into());
                    }
                    let key = format!("{object}:{id}");
                    let previous: Option<GraphRecord> = rows
                        .get(key.as_str())?
                        .map(|bytes| rmp_serde::from_slice(bytes.value()))
                        .transpose()?;
                    if previous.is_none() && next.is_none() {
                        continue;
                    }
                    if let (Some(previous), Some(next)) = (&previous, &mut next) {
                        if previous.properties == next.properties
                            && previous.relationships == next.relationships
                        {
                            next.observed_at_ms = previous.observed_at_ms;
                            if previous.schema_revision == next.schema_revision {
                                continue;
                            }
                        }
                    }
                    let changes = graph_changes(
                        &source_id,
                        &org_id,
                        previous.as_ref(),
                        next.as_ref(),
                        deleted_at_ms,
                    )?;
                    if let Some(schema) = &schema {
                        if previous
                            .as_ref()
                            .is_some_and(|row| row.schema_revision != schema.fingerprint)
                        {
                            pending = pending
                                .checked_sub(1)
                                .context("Salesforce schema progress underflow")?;
                        }
                    }
                    match next {
                        Some(row) => {
                            let row_bytes = rmp_serde::to_vec_named(&row)?;
                            ensure!(
                                row_bytes.len() <= limits.max_projected_row_bytes,
                                "serialized row {key} exceeds max_projected_row_bytes limit"
                            );
                            let previous_len = rows
                                .get(key.as_str())?
                                .map(|existing| existing.value().len())
                                .unwrap_or(0);
                            if row_bytes.len() > previous_len {
                                projected_growth_bytes = projected_growth_bytes
                                    .saturating_add((row_bytes.len() - previous_len) as u64);
                            }
                            rows.insert(key.as_str(), row_bytes.as_slice())?;
                        }
                        None => {
                            rows.remove(key.as_str())?;
                        }
                    }

                    for change in changes {
                        head = head
                            .checked_add(1)
                            .context("Salesforce journal ordinal exhausted")?;
                        let source_position = LocalPosition::new(epoch, head).encode();
                        let stored = StoredEvent {
                            timestamp_ms: timestamp.timestamp_millis(),
                            source_position: source_position.to_vec(),
                            change: change.clone(),
                        };
                        let event_bytes = rmp_serde::to_vec_named(&stored)?;
                        projected_growth_bytes =
                            projected_growth_bytes.saturating_add(event_bytes.len() as u64);
                        events.insert(head, event_bytes.as_slice())?;

                        wrappers.push(Arc::new(SourceEventWrapper {
                            source_id: source_id.clone(),
                            event: SourceEvent::Change(change),
                            timestamp,
                            profiling: None,
                            source_position: Some(source_position),
                            sequence: head,
                        }));
                    }
                }

                ensure!(
                    rows.len()? <= limits.max_rows as u64,
                    "mirrored row count exceeded max_rows limit ({})",
                    limits.max_rows
                );

                prune_events(
                    &mut events,
                    limits.max_events,
                    limits.retention_hours,
                    now,
                    minimum_pinned_ordinal,
                )?;
                ensure!(
                    events.len()? <= limits.max_events as u64,
                    "active Salesforce subscribers prevent pruning within max_events ({})",
                    limits.max_events
                );
                ensure_spool_budget(
                    current_spool_usage,
                    projected_growth_bytes,
                    limits.max_spool_bytes,
                )?;
                let head_bytes = head.to_be_bytes();
                meta.insert(KEY_HEAD, head_bytes.as_slice())?;
                meta.insert(KEY_SCHEMA_PENDING, pending.to_be_bytes().as_slice())?;
                wrappers
            };

            write.commit()?;
            Ok(wrappers)
        })
        .await?
    }

    pub async fn read_events_page(
        &self,
        start_ordinal_inclusive: u64,
        end_ordinal_inclusive: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<Arc<SourceEventWrapper>>> {
        let db = self.db.clone();
        let source_id = self.source_id.clone();
        spawn_blocking(move || -> Result<Vec<Arc<SourceEventWrapper>>> {
            let read = db.begin_read()?;
            let events = read.open_table(EVENTS_TABLE)?;

            let mut wrappers = Vec::new();
            let mut total_bytes = 0usize;
            let mut expected_ordinal = start_ordinal_inclusive;
            for item in events.range(start_ordinal_inclusive..=end_ordinal_inclusive)? {
                let (ordinal, bytes) = item?;
                let ordinal = ordinal.value();
                let raw = bytes.value();

                if ordinal != expected_ordinal {
                    return Err(anyhow!(
                        "durable replay gap detected: expected ordinal {expected_ordinal}, found {ordinal} (requested range {start_ordinal_inclusive}..={end_ordinal_inclusive})",
                    ));
                }
                if !wrappers.is_empty()
                    && (wrappers.len() >= max_events || total_bytes + raw.len() > max_bytes)
                {
                    break;
                }
                total_bytes = total_bytes.saturating_add(raw.len());
                let event: StoredEvent = rmp_serde::from_slice(raw)?;
                let event: StoredEvent = rmp_serde::from_slice(raw)?;
                wrappers.push(Arc::new(SourceEventWrapper {
                    source_id: source_id.clone(),
                    event: SourceEvent::Change(event.change),
                    timestamp: DateTime::from_timestamp_millis(event.timestamp_ms)
                        .ok_or_else(|| anyhow!("stored event timestamp out of range"))?,
                    profiling: None,
                    source_position: Some(Bytes::from(event.source_position)),
                    sequence: ordinal,
                }));
                expected_ordinal = expected_ordinal.saturating_add(1);
            }

            if wrappers.is_empty() && start_ordinal_inclusive <= end_ordinal_inclusive {
                return Err(anyhow!(
                    "durable replay gap detected: no events available for requested range {start_ordinal_inclusive}..={end_ordinal_inclusive}",
                ));
            }

            Ok(wrappers)
        })
        .await?
    }

    pub async fn run_snapshot_reader<F>(
        &self,
        ticket: SnapshotTicket,
        reader: F,
    ) -> Result<BootstrapResult>
    where
        F: Future<Output = Result<BootstrapResult>> + Send + 'static,
    {
        self.join_snapshot_readers(false).await?;
        let mut readers = self.snapshot_readers.lock().await;
        ticket.validate_basic(&self.source_id, Utc::now().timestamp_millis() as u64)?;
        ensure!(
            !readers.contains_key(&ticket.ticket_id),
            "Salesforce snapshot ticket already has a reader"
        );
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let store = self.clone();
        let ticket_id = ticket.ticket_id;
        readers.insert(
            ticket_id.clone(),
            tokio::spawn(async move {
                // A cancelled caller must not release files while a blocking reader still uses them.
                let result = match std::panic::AssertUnwindSafe(reader).catch_unwind().await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("Salesforce snapshot reader panicked")),
                };
                let cleanup = store.release_snapshot_ticket(&ticket_id).await;
                let result = match (result, cleanup) {
                    (Ok(result), Ok(())) => Ok(result),
                    (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                    (Err(error), Err(cleanup)) => Err(error.context(format!(
                        "Salesforce snapshot cleanup also failed: {cleanup:#}"
                    ))),
                };
                if let Err(error) = &result {
                    log::error!("Salesforce snapshot reader failed: {error:#}");
                }
                let _ = result_tx.send(result);
            }),
        );
        drop(readers);
        result_rx
            .await
            .context("Salesforce snapshot reader stopped without a result")?
    }

    pub async fn finish_snapshot_readers(&self) -> Result<()> {
        self.join_snapshot_readers(true).await
    }

    pub async fn has_snapshot_readers(&self) -> bool {
        self.snapshot_readers
            .lock()
            .await
            .values()
            .any(|task| !task.is_finished())
    }

    async fn join_snapshot_readers(&self, wait: bool) -> Result<()> {
        let mut readers = self.snapshot_readers.lock().await;
        let tickets: Vec<_> = readers
            .iter()
            .filter(|(_, task)| wait || task.is_finished())
            .map(|(ticket, _)| ticket.clone())
            .collect();
        for ticket in tickets {
            let result = readers
                .get_mut(&ticket)
                .context("missing Salesforce snapshot reader")?
                .await;
            readers.remove(&ticket);
            result.context("Salesforce snapshot reader task failed")?;
        }
        Ok(())
    }

    async fn release_snapshot_ticket(&self, ticket_id: &str) -> Result<()> {
        let db = self.db.clone();
        let snapshots_root = self.snapshots_dir.clone();
        let budget = self.spool_budget.clone();
        let ticket_id = ticket_id.to_string();
        spawn_blocking(move || -> Result<()> {
            let write = db.begin_write()?;
            {
                let mut leases = write.open_table(LEASES_TABLE)?;
                leases.remove(ticket_id.as_str())?;
            }
            write.commit()?;
            budget
                .remove_snapshot(&snapshots_root, &snapshots_root.join(&ticket_id))
                .context("failed to remove completed Salesforce snapshot")
        })
        .await?
    }

    pub async fn create_snapshot_ticket(&self, ticket_ttl_seconds: u64) -> Result<SnapshotTicket> {
        self.join_snapshot_readers(false).await?;
        let mut export = self.snapshot_export.lock().await;
        if let Some(previous) = export.as_mut() {
            let result = previous.await;
            *export = None;
            result.context("cancelled caller's Salesforce snapshot task failed")??;
        }
        let now_ms = Utc::now().timestamp_millis() as u64;
        self.cleanup_expired_leases(now_ms).await?;
        self.ensure_lease_capacity().await?;
        let store = self.clone();
        *export = Some(spawn_blocking(move || {
            store.export_snapshot(ticket_ttl_seconds, now_ms)
        }));
        let result = export
            .as_mut()
            .context("missing Salesforce snapshot task")?
            .await;
        *export = None;
        result.context("Salesforce snapshot export task failed")?
    }

    pub async fn has_snapshot_export(&self) -> bool {
        match self.snapshot_export.try_lock() {
            Ok(export) => export.is_some(),
            Err(_) => true,
        }
    }

    pub async fn finish_snapshot_export(&self) -> Result<()> {
        let mut export = self.snapshot_export.lock().await;
        if let Some(task) = export.as_mut() {
            let result = task.await;
            *export = None;
            result.context("Salesforce snapshot export task failed")??;
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn expire_unpinned_events(&self) -> Result<()> {
        let db = self.db.clone();
        let limits = self.limits.clone();
        spawn_blocking(move || -> Result<()> {
            let write = db.begin_write()?;
            {
                let mut events = write.open_table(EVENTS_TABLE)?;
                let future =
                    Utc::now().timestamp_millis() + (limits.retention_hours as i64 + 1) * 3_600_000;
                prune_events(
                    &mut events,
                    limits.max_events,
                    limits.retention_hours,
                    future,
                    None,
                )?;
            }
            write.commit()?;
            Ok(())
        })
        .await?
    }

    fn export_snapshot(&self, ticket_ttl_seconds: u64, now_ms: u64) -> Result<SnapshotTicket> {
        let ticket_id = Uuid::new_v4().to_string();
        let ticket_dir = self.snapshots_dir.join(&ticket_id);
        let temp_dir = self.snapshots_dir.join(format!("{ticket_id}.tmp"));
        let rows_rel = format!("{SNAPSHOT_DIR}/{ticket_id}/rows.msgpack");
        let manifest_rel = format!("{SNAPSHOT_DIR}/{ticket_id}/manifest.msgpack");
        let result = (|| -> Result<SnapshotTicket> {
            let usage = self.spool_budget.usage()?;
            let read = self.db.begin_read()?;
            let meta = read.open_table(META_TABLE)?;
            let head_ordinal =
                decode_u64(meta.get(KEY_HEAD)?.context("missing head ordinal")?.value())?;
            let rows = read.open_table(ROWS_TABLE)?;
            std::fs::create_dir(&temp_dir)?;
            let rows_path = temp_dir.join("rows.msgpack");
            let mut writer = BufWriter::new(self.spool_budget.create_file(&rows_path)?);
            let mut bytes_written = 0u64;
            let mut row_count = 0u64;
            for entry in rows.iter()? {
                let (_, bytes) = entry?;
                let projected = bytes_written
                    .checked_add(bytes.value().len() as u64 + 4)
                    .context("snapshot size overflow")?;
                ensure!(
                    projected <= MAX_SNAPSHOT_FILE_BYTES,
                    "Salesforce snapshot exceeds 512 MiB"
                );
                ensure_spool_budget(usage, projected, self.limits.max_spool_bytes)?;
                let row: GraphRecord = rmp_serde::from_slice(bytes.value())?;
                let record_bytes =
                    write_snapshot_record(&mut writer, &row, self.limits.max_projected_row_bytes)?;
                bytes_written = bytes_written
                    .checked_add(record_bytes as u64)
                    .context("snapshot size overflow")?;
                ensure!(
                    bytes_written <= MAX_SNAPSHOT_FILE_BYTES,
                    "Salesforce snapshot exceeds 512 MiB"
                );
                ensure_spool_budget(usage, bytes_written, self.limits.max_spool_bytes)?;
                row_count += 1;
            }
            writer.flush()?;
            writer.get_ref().file.sync_all()?;
            drop(writer);
            let manifest = SnapshotManifest {
                version: SNAPSHOT_MANIFEST_VERSION,
                source_id: self.source_id.clone(),
                source_identity: self.source_identity.clone(),
                org_id: self.org_id.clone(),
                objects: self.objects.clone(),
                epoch: self.epoch,
                head_ordinal,
                created_at_ms: now_ms,
                rows_rel_path: rows_rel,
                row_count,
                rows_sha256: file_sha256(&rows_path)?,
            };
            let manifest_bytes = rmp_serde::to_vec_named(&manifest)?;
            ensure_spool_budget(
                usage,
                bytes_written + manifest_bytes.len() as u64,
                self.limits.max_spool_bytes,
            )?;
            publish_snapshot_files(
                &self.root_dir,
                &temp_dir,
                &ticket_dir,
                &manifest_bytes,
                &self.spool_budget,
            )?;
            let ticket = SnapshotTicket {
                version: SNAPSHOT_MANIFEST_VERSION,
                ticket_id: ticket_id.clone(),
                source_id: self.source_id.clone(),
                source_identity: self.source_identity.clone(),
                org_id: self.org_id.clone(),
                objects: self.objects.clone(),
                epoch: self.epoch,
                head_ordinal,
                manifest_rel_path: manifest_rel.clone(),
                expires_at_ms: now_ms.saturating_add(ticket_ttl_seconds.saturating_mul(1000)),
                checksum_sha256: sha256_hex(&manifest_bytes),
            };
            let lease = LeaseRecord {
                ticket_id: ticket_id.clone(),
                manifest_rel_path: manifest_rel.clone(),
                expires_at_ms: ticket.expires_at_ms,
            };
            let write = self.db.begin_write()?;
            {
                let mut leases = write.open_table(LEASES_TABLE)?;
                ensure!(
                    leases.len()? < self.limits.max_snapshot_leases as u64,
                    "snapshot lease limit exceeded"
                );
                leases.insert(
                    ticket_id.as_str(),
                    rmp_serde::to_vec_named(&lease)?.as_slice(),
                )?;
            }
            write.commit()?;
            Ok(ticket)
        })();
        if result.is_err() {
            for path in [&temp_dir, &ticket_dir] {
                if let Err(error) = self.spool_budget.remove_snapshot(&self.snapshots_dir, path) {
                    log::error!(
                        "Failed to clean rejected Salesforce snapshot {}: {error:#}",
                        path.display()
                    );
                }
            }
        }
        result
    }

    pub async fn cleanup_expired_leases(&self, now_ms: u64) -> Result<()> {
        let db = self.db.clone();
        let snapshots_root = self.snapshots_dir.clone();
        let budget = self.spool_budget.clone();
        let readers = self.snapshot_readers.clone().lock_owned().await;
        spawn_blocking(move || -> Result<()> {
            let write = db.begin_write()?;
            let mut expired = Vec::new();
            {
                let mut leases = write.open_table(LEASES_TABLE)?;
                let keys = leases
                    .iter()?
                    .map(|entry| {
                        let (key, value) = entry?;
                        let lease: LeaseRecord = rmp_serde::from_slice(value.value())?;
                        Ok((key.value().to_string(), lease))
                    })
                    .collect::<Result<Vec<_>>>()?;
                for (key, lease) in keys {
                    if lease.expires_at_ms < now_ms && !readers.contains_key(&key) {
                        leases.remove(key.as_str())?;
                        expired.push(lease);
                    }
                }
            }
            write.commit()?;
            for lease in expired {
                let ticket_dir = snapshots_root.join(&lease.ticket_id);
                budget.remove_snapshot(&snapshots_root, &ticket_dir)?;
            }
            Ok(())
        })
        .await?
    }

    async fn ensure_lease_capacity(&self) -> Result<()> {
        let db = self.db.clone();
        let max = self.limits.max_snapshot_leases;
        spawn_blocking(move || -> Result<()> {
            let read = db.begin_read()?;
            let leases = read.open_table(LEASES_TABLE)?;
            ensure!(
                leases.len()? < max as u64,
                "maximum concurrent snapshot leases ({max}) reached"
            );
            Ok(())
        })
        .await?
    }

    pub async fn deprovision(&self) -> Result<()> {
        self.finish_snapshot_export().await?;
        self.finish_snapshot_readers().await?;
        let ownership_path = self.root_dir.join(OWNERSHIP_FILE);
        if !ownership_path.exists() {
            return Ok(());
        }
        let ownership: OwnershipFile = serde_json::from_slice(&std::fs::read(&ownership_path)?)?;
        ensure!(
            ownership.source_id == self.source_id
                && ownership.source_identity == self.source_identity,
            "refusing to deprovision a spool root that is not owned by this source identity"
        );

        safe_remove_owned_path(&self.root_dir, &self.root_dir.join(SNAPSHOT_DIR))
            .context("failed to remove Salesforce snapshot exports")?;
        safe_remove_owned_file(&self.root_dir, &self.root_dir.join(DB_FILE))
            .context("failed to remove Salesforce durable mirror database")?;
        safe_remove_owned_file(&self.root_dir, &ownership_path)
            .context("failed to remove Salesforce ownership marker")?;
        safe_remove_owned_file(&self.root_dir, &self.root_dir.join("schema.json"))?;
        safe_remove_owned_file(&self.root_dir, &self.root_dir.join("schema.json.tmp"))?;
        Ok(())
    }
}

fn validate_meta(
    meta: &redb::Table<&str, &[u8]>,
    source_id: &str,
    source_identity: &str,
) -> Result<()> {
    let version = decode_u32(
        meta.get(KEY_FORMAT_VERSION)?
            .context("missing format_version metadata")?
            .value(),
    )?;
    ensure!(
        version == FORMAT_VERSION,
        "unsupported durable mirror format_version {version}; use a new v2 spool and query state directory; existing state is preserved"
    );

    let source_id_guard = meta
        .get(KEY_SOURCE_ID)?
        .context("missing source_id metadata")?;
    let stored_source_id = std::str::from_utf8(source_id_guard.value())?;
    ensure!(
        stored_source_id == source_id,
        "source_id mismatch in durable mirror metadata"
    );

    let source_identity_guard = meta
        .get(KEY_SOURCE_IDENTITY)?
        .context("missing source_identity metadata")?;
    let stored_identity = std::str::from_utf8(source_identity_guard.value())?;
    ensure!(
        stored_identity == source_identity,
        "source identity mismatch in durable mirror metadata"
    );

    Ok(())
}

fn decode_u32(bytes: &[u8]) -> Result<u32> {
    ensure!(bytes.len() == 4, "expected 4 bytes");
    let mut out = [0u8; 4];
    out.copy_from_slice(bytes);
    Ok(u32::from_be_bytes(out))
}

fn decode_u64(bytes: &[u8]) -> Result<u64> {
    ensure!(bytes.len() == 8, "expected 8 bytes");
    let mut out = [0u8; 8];
    out.copy_from_slice(bytes);
    Ok(u64::from_be_bytes(out))
}

fn decode_u128(bytes: &[u8]) -> Result<u128> {
    ensure!(bytes.len() == 16, "expected 16 bytes");
    let mut out = [0u8; 16];
    out.copy_from_slice(bytes);
    Ok(u128::from_be_bytes(out))
}

fn ensure_owned_root(
    path: &Path,
    source_id: &str,
    source_identity: &str,
    budget: &Arc<SpoolBudget>,
) -> Result<()> {
    if path.exists() {
        let ownership: OwnershipFile = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            ownership.format_version == FORMAT_VERSION,
            "Salesforce spool format {} is unsupported; use a new v2 spool and query state directory; existing state is preserved",
            ownership.format_version
        );
        ensure!(
            ownership.source_id == source_id,
            "Salesforce spool root already belongs to a different source_id"
        );
        ensure!(
            ownership.source_identity == source_identity,
            "Salesforce spool root source_identity mismatch"
        );
        return Ok(());
    }

    let ownership = OwnershipFile {
        format_version: FORMAT_VERSION,
        source_id: source_id.to_owned(),
        source_identity: source_identity.to_owned(),
    };

    let temp = path.with_extension("tmp");
    budget.write_file(&temp, &serde_json::to_vec_pretty(&ownership)?)?;
    budget.rename(&temp, path)?;
    Ok(())
}

fn prune_events(
    events: &mut redb::Table<u64, &[u8]>,
    max_events: usize,
    retention_hours: u64,
    now_ms: i64,
    minimum_pinned_ordinal: Option<u64>,
) -> Result<()> {
    let is_pinned = |ordinal: u64| -> bool {
        minimum_pinned_ordinal
            .map(|pinned_start| ordinal >= pinned_start)
            .unwrap_or(false)
    };

    while events.len()? > max_events as u64 {
        let key = if let Some(entry) = events.first()? {
            entry.0.value()
        } else {
            break;
        };
        if is_pinned(key) {
            break;
        }
        events.remove(key)?;
    }

    if retention_hours == 0 {
        return Ok(());
    }
    let cutoff = now_ms.saturating_sub((retention_hours as i64) * 60 * 60 * 1000);
    while let Some((key, raw)) = ({
        let first = events.first()?;
        first.map(|entry| (entry.0.value(), entry.1.value().to_vec()))
    }) {
        if is_pinned(key) {
            break;
        }
        let stored: StoredEvent = rmp_serde::from_slice(&raw)?;
        if stored.timestamp_ms >= cutoff {
            break;
        }
        events.remove(key)?;
    }
    Ok(())
}

fn publish_snapshot_files(
    root_dir: &Path,
    temp_dir: &Path,
    ticket_dir: &Path,
    manifest_bytes: &[u8],
    budget: &Arc<SpoolBudget>,
) -> Result<()> {
    let temp_manifest = temp_dir.join("manifest.msgpack");
    budget.write_file(&temp_manifest, manifest_bytes)?;

    std::fs::File::open(&temp_manifest)?.sync_all()?;
    std::fs::File::open(temp_dir)?.sync_all()?;

    ensure!(!ticket_dir.exists(), "Salesforce snapshot ticket collision");
    budget.rename(temp_dir, ticket_dir)?;

    // fsync parent directories for atomic publish visibility.
    std::fs::File::open(ticket_dir)?.sync_all()?;
    std::fs::File::open(root_dir.join(SNAPSHOT_DIR))?.sync_all()?;
    std::fs::File::open(root_dir)?.sync_all()?;

    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn cleanup_orphan_snapshots(db: &Database, snapshots: &Path) -> Result<()> {
    let read = db.begin_read()?;
    let leases = read.open_table(LEASES_TABLE)?;
    for entry in std::fs::read_dir(snapshots)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let identifier = name.strip_suffix(".tmp").unwrap_or(name);
        if Uuid::parse_str(identifier).is_ok() && leases.get(identifier)?.is_none() {
            ensure!(
                entry.file_type()?.is_dir(),
                "unexpected Salesforce snapshot artifact {}",
                entry.path().display()
            );
            safe_remove_owned_path(snapshots, &entry.path())?;
        }
    }
    Ok(())
}

fn safe_remove_owned_path(owner_root: &Path, target: &Path) -> Result<()> {
    let owner_root = owner_root.canonicalize()?;
    if !target.exists() {
        return Ok(());
    }
    let canonical_parent = target
        .parent()
        .context("snapshot target has no parent")?
        .canonicalize()?;
    ensure!(
        canonical_parent.starts_with(&owner_root),
        "refusing to delete snapshot path outside owned snapshot root"
    );
    std::fs::remove_dir_all(target)?;
    Ok(())
}

fn safe_remove_owned_file(owner_root: &Path, target: &Path) -> Result<()> {
    let owner_root = owner_root.canonicalize()?;
    if !target.exists() {
        return Ok(());
    }
    let canonical_parent = target
        .parent()
        .context("owned file target has no parent")?
        .canonicalize()?;
    ensure!(
        canonical_parent.starts_with(&owner_root),
        "refusing to delete file path outside owned source root"
    );
    let metadata = std::fs::symlink_metadata(target)?;
    ensure!(
        !metadata.is_dir(),
        "refusing to delete directory via owned file cleanup path"
    );
    std::fs::remove_file(target)?;
    Ok(())
}

fn compute_spool_usage_bytes(root_dir: &Path) -> Result<u64> {
    if !root_dir.exists() {
        return Ok(0);
    }

    let mut total = 0u64;
    let mut stack = vec![root_dir.to_path_buf()];

    while let Some(path) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("failed to stat spool path {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "spool usage accounting refuses symlink path {}",
                path.display()
            ));
        }

        if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| anyhow!("spool usage accounting overflowed u64"))?;
            continue;
        }

        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)
                .with_context(|| format!("failed to read spool directory {}", path.display()))?
            {
                let entry = entry?;
                stack.push(entry.path());
            }
        }
    }

    Ok(total)
}

fn ensure_spool_budget(current: u64, additional: u64, max: u64) -> Result<()> {
    let projected = current.saturating_add(additional);
    ensure!(
        projected <= max,
        "aggregate spool usage would exceed max_spool_bytes limit (max={max} bytes, current={current}, additional={additional}, projected={projected})"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::ElementValue;
    use drasi_salesforce_common::{ObjectPlan, Properties};
    use tempfile::TempDir;

    fn test_limits(max_rows: usize) -> StorageLimits {
        StorageLimits {
            max_rows,
            max_events: 128,
            retention_hours: 24,
            max_projected_row_bytes: 1024 * 1024,
            max_snapshot_leases: 8,
            max_spool_bytes: 8 * 1024 * 1024 * 1024,
        }
    }

    fn row(id: &str, name: &str, timestamp: u64) -> GraphRecord {
        let mut properties = Properties::new();
        properties.insert("Name".to_string(), ElementValue::String(name.into()));
        GraphRecord {
            object: "Account".into(),
            id: id.to_string(),
            properties,
            observed_at_ms: timestamp,
            relationships: Vec::new(),
            schema_revision: String::new(),
        }
    }

    fn insert_mutation(
        _source_id: &str,
        _org_id: &str,
        _plan: &ObjectPlan,
        row: GraphRecord,
    ) -> RecordMutation {
        RecordMutation::Upsert(row)
    }

    #[tokio::test]
    async fn regression_missing_record_deletes_are_idempotent() -> Result<()> {
        let temp = TempDir::new()?;
        let store = DurableMirrorStore::open(
            temp.path().into(),
            "source".into(),
            "identity".into(),
            "00D000000000001AAA".into(),
            vec!["Account".into()],
            test_limits(100),
        )
        .await?;
        let delete = RecordMutation::Delete {
            object: "Account".into(),
            id: "001000000000001AAA".into(),
            observed_at_ms: 2,
            schema_revision: String::new(),
        };
        assert!(store
            .commit_mutations(vec![delete.clone()])
            .await?
            .is_empty());
        assert_eq!(store.head_ordinal().await?, 0);
        store
            .commit_mutations(vec![RecordMutation::Upsert(row(
                "001000000000001AAA",
                "Account",
                1,
            ))])
            .await?;
        assert_eq!(store.commit_mutations(vec![delete.clone()]).await?.len(), 1);
        assert!(store.commit_mutations(vec![delete]).await?.is_empty());
        assert_eq!(store.head_ordinal().await?, 2);
        assert!(store.load_all_rows().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn spool_budget_serializes_database_and_export_growth() -> Result<()> {
        let temp = TempDir::new()?;
        let budget = Arc::new(SpoolBudget {
            root: temp.path().into(),
            max_bytes: 12 * 1024,
            growth: StdMutex::new(()),
        });
        let db = BudgetedBackend {
            file: FileBackend::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(temp.path().join(DB_FILE))?,
            )?,
            budget: budget.clone(),
        };
        let mut export = budget.create_file(&temp.path().join("export.msgpack"))?;
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let db_barrier = barrier.clone();
        let database = spawn_blocking(move || {
            db_barrier.wait();
            db.set_len(8192)
        });
        let snapshot = spawn_blocking(move || {
            barrier.wait();
            export.write_all(&[0; 8192])
        });
        let (database, snapshot) = tokio::join!(database, snapshot);
        let results = [database?, snapshot?];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(compute_spool_usage_bytes(temp.path())?, 8192);
        assert!(results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| error.to_string().contains("max_spool_bytes")));
        Ok(())
    }

    #[tokio::test]
    async fn spool_budget_bounds_database_offset_writes_and_new_store_creation() -> Result<()> {
        let temp = TempDir::new()?;
        let budget = Arc::new(SpoolBudget {
            root: temp.path().into(),
            max_bytes: 1024,
            growth: StdMutex::new(()),
        });
        let db = BudgetedBackend {
            file: FileBackend::new(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(temp.path().join("test.redb"))?,
            )?,
            budget: budget.clone(),
        };
        db.write(512, &[0; 512])?;
        assert!(db.write(1024, &[1]).is_err());
        assert!(budget
            .write_file(&temp.path().join("schema.json.tmp"), b"{}")
            .is_err());
        assert_eq!(compute_spool_usage_bytes(temp.path())?, 1024);
        let root = temp.path().join("new-store");
        let result = DurableMirrorStore::open(
            root.clone(),
            "source".into(),
            "identity".into(),
            "00D000000000001AAA".into(),
            vec!["Account".into()],
            StorageLimits {
                max_spool_bytes: 1024,
                ..test_limits(100)
            },
        )
        .await;
        assert!(result.is_err());
        assert!(compute_spool_usage_bytes(&root)? <= 1024);
        Ok(())
    }

    #[tokio::test]
    async fn regression_database_growth_obeys_physical_spool_limit() -> Result<()> {
        let temp = TempDir::new()?;
        let mut limits = StorageLimits {
            max_events: 10_000,
            ..test_limits(10_000)
        };
        let open = |limits| {
            DurableMirrorStore::open(
                temp.path().into(),
                "source".into(),
                "identity".into(),
                "00D000000000001AAA".into(),
                vec!["Account".into()],
                limits,
            )
        };
        drop(open(limits.clone()).await?);
        limits.max_spool_bytes = compute_spool_usage_bytes(temp.path())? + 64 * 1024;
        let store = open(limits.clone()).await?;
        let epoch = store.epoch();
        let mut committed = 0;
        let mut rejected = false;
        for index in 0..2000 {
            let result = store
                .commit_mutations(vec![RecordMutation::Upsert(row(
                    &format!("001{index:012}AAA"),
                    &"x".repeat(2048),
                    1,
                ))])
                .await;
            let usage = compute_spool_usage_bytes(temp.path())?;
            assert!(
                usage <= limits.max_spool_bytes,
                "{usage} > {}",
                limits.max_spool_bytes
            );
            match result {
                Ok(events) => committed += events.len() as u64,
                Err(error) => {
                    assert!(
                        format!("{error:#}").contains("max_spool_bytes"),
                        "{error:#}"
                    );
                    rejected = true;
                    break;
                }
            }
        }
        assert!(
            rejected,
            "fixture must exercise database allocation rejection"
        );
        drop(store);
        let reopened = open(limits.clone()).await?;
        assert_eq!(reopened.epoch(), epoch);
        assert_eq!(reopened.head_ordinal().await?, committed);
        assert_eq!(reopened.load_all_rows().await?.len() as u64, committed);
        let replay = reopened
            .read_events_page(1, committed + 1, 2001, 8 * 1024 * 1024)
            .await?;
        assert_eq!(replay.len() as u64, committed);
        assert!(replay
            .iter()
            .enumerate()
            .all(|(index, event)| event.sequence == index as u64 + 1));
        drop(reopened);
        limits.max_spool_bytes *= 2;
        let expanded = open(limits).await?;
        let events = expanded
            .commit_mutations(vec![RecordMutation::Upsert(row(
                "001000000002001AAA",
                "After capacity increase",
                2,
            ))])
            .await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, committed + 1);
        Ok(())
    }

    #[tokio::test]
    async fn commit_is_atomic_when_limits_reject_batch() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(1),
        )
        .await
        .expect("open store");

        store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000001AAA", "Acme", 1_700_000_000_000),
            )])
            .await
            .expect("initial commit should succeed");

        let error = store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000002AAA", "Globex", 1_700_000_100_000),
            )])
            .await
            .expect_err("second commit should fail max_rows check");
        assert!(
            format!("{error:#}").contains("max_rows"),
            "unexpected error: {error:#}"
        );

        let rows = store.load_all_rows().await.expect("load rows");
        assert_eq!(
            rows.len(),
            1,
            "failed commit must not partially persist rows"
        );
        assert_eq!(rows[0].id, "001000000000001AAA");
        assert_eq!(store.head_ordinal().await.expect("head"), 1);
        let page = store
            .read_events_page(1, 16, 32, 1024 * 1024)
            .await
            .expect("read events");
        assert_eq!(
            page.len(),
            1,
            "failed commit must not partially append events"
        );
    }

    #[tokio::test]
    async fn reopen_preserves_epoch_rows_and_head() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let first_epoch;
        {
            let store = DurableMirrorStore::open(
                temp.path().to_path_buf(),
                source_id.to_string(),
                "identity-v1".to_string(),
                org_id.to_string(),
                vec!["Account".to_string()],
                test_limits(100),
            )
            .await
            .expect("open initial store");

            first_epoch = store.epoch();
            store
                .commit_mutations(vec![insert_mutation(
                    source_id,
                    org_id,
                    &plan,
                    row("001000000000001AAA", "Acme", 1_700_000_000_000),
                )])
                .await
                .expect("commit");
            assert_eq!(store.head_ordinal().await.expect("head"), 1);
        }

        let reopened = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(100),
        )
        .await
        .expect("reopen store");

        assert_eq!(reopened.epoch(), first_epoch, "epoch should persist");
        assert_eq!(reopened.head_ordinal().await.expect("head"), 1);
        let rows = reopened.load_all_rows().await.expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "001000000000001AAA");

        let ticket = reopened
            .create_snapshot_ticket(60)
            .await
            .expect("create snapshot ticket");
        assert_eq!(ticket.head_ordinal, 1);
        assert_eq!(ticket.epoch, first_epoch);
    }

    #[tokio::test]
    async fn open_rejects_identity_mismatch() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";

        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(100),
        )
        .await
        .expect("open initial store");
        drop(store);

        let result = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "different-identity".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(100),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("identity mismatch must be rejected"),
            Err(error) => error,
        };

        assert!(
            format!("{error:#}").contains("source_identity mismatch"),
            "unexpected mismatch error: {error:#}"
        );
    }

    #[tokio::test]
    async fn commit_rejects_when_aggregate_spool_budget_would_be_exceeded() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let baseline_store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(100),
        )
        .await
        .expect("open baseline store");

        baseline_store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000001AAA", "Acme", 1_700_000_000_000),
            )])
            .await
            .expect("seed commit should succeed");
        drop(baseline_store);

        let current_usage =
            compute_spool_usage_bytes(temp.path()).expect("measure baseline spool usage");
        let strict_limits = StorageLimits {
            max_spool_bytes: current_usage,
            ..test_limits(100)
        };

        let strict_store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            strict_limits,
        )
        .await
        .expect("reopen strict store");

        let error = strict_store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000002AAA", "Globex", 1_700_000_100_000),
            )])
            .await
            .expect_err("commit should fail when max_spool_bytes is exhausted");

        assert!(
            format!("{error:#}").contains("max_spool_bytes"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            strict_store.head_ordinal().await.expect("head"),
            1,
            "failed commit must not advance head ordinal"
        );
    }

    #[tokio::test]
    async fn deprovision_removes_only_owned_artifacts() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let unrelated_file = temp.path().join("keep-me.txt");
        std::fs::write(&unrelated_file, b"do not delete").expect("write unrelated file");

        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(100),
        )
        .await
        .expect("open store");

        store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000001AAA", "Acme", 1_700_000_000_000),
            )])
            .await
            .expect("seed commit");
        let _ticket = store
            .create_snapshot_ticket(60)
            .await
            .expect("snapshot export");

        store.deprovision().await.expect("deprovision store");

        assert!(
            unrelated_file.exists(),
            "deprovision should not remove unrelated files in spool parent"
        );
        assert!(
            !temp.path().join(DB_FILE).exists(),
            "deprovision should remove owned redb file"
        );
        assert!(
            !temp.path().join(SNAPSHOT_DIR).exists(),
            "deprovision should remove owned snapshots directory"
        );
        assert!(
            !temp.path().join(OWNERSHIP_FILE).exists(),
            "deprovision should remove ownership marker"
        );
    }

    #[tokio::test]
    async fn snapshot_ticket_creation_respects_aggregate_spool_budget() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();

        let mut store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            test_limits(100),
        )
        .await
        .expect("open store");

        store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000001AAA", "Acme", 1_700_000_000_000),
            )])
            .await
            .expect("seed commit");
        store.limits.max_spool_bytes = 1;

        let error = store
            .create_snapshot_ticket(60)
            .await
            .expect_err("snapshot export must fail when aggregate spool budget is exhausted");
        assert!(
            format!("{error:#}").contains("max_spool_bytes"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn cleanup_expired_leases_removes_snapshot_dirs_and_frees_capacity() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let limits = StorageLimits {
            max_snapshot_leases: 1,
            ..test_limits(100)
        };
        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            limits,
        )
        .await
        .expect("open store");

        store
            .commit_mutations(vec![insert_mutation(
                source_id,
                org_id,
                &plan,
                row("001000000000001AAA", "Acme", 1_700_000_000_000),
            )])
            .await
            .expect("seed commit");

        let first_ticket = store
            .create_snapshot_ticket(60)
            .await
            .expect("first snapshot ticket");
        let first_ticket_dir = temp.path().join(SNAPSHOT_DIR).join(&first_ticket.ticket_id);
        assert!(
            first_ticket_dir.exists(),
            "first snapshot ticket directory should exist"
        );

        store
            .cleanup_expired_leases(u64::MAX)
            .await
            .expect("force cleanup expired leases");
        assert!(
            !first_ticket_dir.exists(),
            "expired lease cleanup should remove ticket directory"
        );

        let second_ticket = store
            .create_snapshot_ticket(60)
            .await
            .expect("lease capacity should be reusable after cleanup");
        assert!(
            temp.path()
                .join(SNAPSHOT_DIR)
                .join(&second_ticket.ticket_id)
                .exists(),
            "new snapshot ticket should be created after cleanup frees lease capacity"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finished_snapshot_readers_release_leases_on_success_error_and_panic() -> Result<()> {
        let temp = TempDir::new()?;
        let store = DurableMirrorStore::open(
            temp.path().into(),
            "source".into(),
            "identity".into(),
            "00D000000000001AAA".into(),
            vec!["Account".into()],
            StorageLimits {
                max_snapshot_leases: 1,
                ..test_limits(1)
            },
        )
        .await?;
        for outcome in 0..3 {
            let ticket = store.create_snapshot_ticket(60).await?;
            let directory = store.snapshots_dir.join(&ticket.ticket_id);
            let result = store
                .run_snapshot_reader(ticket, async move {
                    match outcome {
                        0 => Ok(BootstrapResult {
                            event_count: 0,
                            source_position: None,
                        }),
                        1 => Err(anyhow!("injected snapshot reader failure")),
                        _ => panic!("injected snapshot reader panic"),
                    }
                })
                .await;
            match outcome {
                0 => assert_eq!(result?.event_count, 0),
                1 => assert!(format!("{:#}", result.unwrap_err())
                    .contains("injected snapshot reader failure")),
                _ => assert!(
                    format!("{:#}", result.unwrap_err()).contains("snapshot reader panicked")
                ),
            }
            assert!(
                !directory.exists(),
                "completed reader must release its export"
            );
        }
        store.finish_snapshot_readers().await?;
        store.create_snapshot_ticket(60).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_snapshot_waiter_keeps_blocking_reader_owned_until_cleanup() -> Result<()> {
        let temp = TempDir::new()?;
        let store = DurableMirrorStore::open(
            temp.path().into(),
            "source".into(),
            "identity".into(),
            "00D000000000001AAA".into(),
            vec!["Account".into()],
            StorageLimits {
                max_snapshot_leases: 1,
                ..test_limits(1)
            },
        )
        .await?;
        let ticket = store.create_snapshot_ticket(60).await?;
        let directory = store.snapshots_dir.join(&ticket.ticket_id);
        let path = directory.join("rows.msgpack");
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let reader_store = store.clone();
        let waiter = tokio::spawn(async move {
            reader_store
                .run_snapshot_reader(ticket, async move {
                    spawn_blocking(move || -> Result<BootstrapResult> {
                        let mut file = std::fs::File::open(path)?;
                        entered_tx
                            .send(())
                            .map_err(|_| anyhow!("reader observer stopped"))?;
                        release_rx.blocking_recv()?;
                        assert_eq!(file.read(&mut [0])?, 0);
                        Ok(BootstrapResult {
                            event_count: 0,
                            source_position: None,
                        })
                    })
                    .await?
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx).await??;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        store.cleanup_expired_leases(u64::MAX).await?;
        assert!(
            directory.exists(),
            "expiry must not delete an active reader's files"
        );
        assert!(store.create_snapshot_ticket(60).await.is_err());

        let joining_store = store.clone();
        let joining = tokio::spawn(async move { joining_store.finish_snapshot_readers().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.snapshot_readers.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        joining.abort();
        assert!(joining.await.unwrap_err().is_cancelled());
        assert!(store.has_snapshot_readers().await);

        release_tx
            .send(())
            .map_err(|_| anyhow!("blocking reader ended early"))?;
        store.finish_snapshot_readers().await?;
        assert!(!directory.exists());
        store.create_snapshot_ticket(60).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_cleanup_does_not_race_mutation_spool_accounting() -> Result<()> {
        let temp = TempDir::new()?;
        let store = DurableMirrorStore::open(
            temp.path().into(),
            "source".into(),
            "identity".into(),
            "00D000000000001AAA".into(),
            vec!["Account".into()],
            StorageLimits {
                max_snapshot_leases: 1,
                ..test_limits(1)
            },
        )
        .await?;
        tokio::try_join!(
            async {
                for number in 0..64 {
                    store
                        .commit_mutations(vec![RecordMutation::Upsert(row(
                            "001000000000001AAA",
                            &format!("Account {number}"),
                            number,
                        ))])
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            },
            async {
                for _ in 0..64 {
                    let ticket = store.create_snapshot_ticket(60).await?;
                    store
                        .run_snapshot_reader(ticket, async {
                            Ok(BootstrapResult {
                                event_count: 0,
                                source_position: None,
                            })
                        })
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
        )?;
        store.finish_snapshot_readers().await?;
        assert_eq!(store.head_ordinal().await?, 64);
        assert!(std::fs::read_dir(&store.snapshots_dir)?.next().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn pruning_keeps_events_required_by_active_replay_pin() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let limits = StorageLimits {
            max_events: 3,
            ..test_limits(100)
        };
        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            limits,
        )
        .await
        .expect("open store");

        for ordinal in 1..=3u64 {
            store
                .commit_mutations(vec![insert_mutation(
                    source_id,
                    org_id,
                    &plan,
                    row(
                        &format!("0010000000000{ordinal:02}AAA"),
                        &format!("Seed {ordinal}"),
                        ordinal,
                    ),
                )])
                .await
                .expect("seed commit");
        }

        let pin = store.register_replay_pin(1, 3).await.expect("pin");
        for ordinal in 4..=6u64 {
            let committed = store
                .commit_mutations(vec![insert_mutation(
                    source_id,
                    org_id,
                    &plan,
                    row(
                        &format!("0010000000000{ordinal:02}AAA"),
                        &format!("Tail {ordinal}"),
                        ordinal,
                    ),
                )])
                .await;
            if ordinal == 4 {
                committed.expect("one unpinned event can be pruned");
            } else {
                assert!(
                    committed.is_err(),
                    "retention pressure must fail atomically"
                );
            }
        }
        assert_eq!(store.head_ordinal().await.expect("head"), 4);

        // Even after max_events pressure, the pinned replay range remains readable.
        let retained = store
            .read_events_page(2, 3, 10, 1024 * 1024)
            .await
            .expect("pinned replay range must remain available");
        assert_eq!(retained.len(), 2);
        assert_eq!(retained[0].sequence, 2);
        assert_eq!(retained[1].sequence, 3);

        drop(pin);
    }

    #[tokio::test]
    async fn bootstrap_confirmation_pin_advances_without_dropping_live_subscription() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.into(),
            "identity-v1".into(),
            org_id.into(),
            vec!["Account".into()],
            StorageLimits {
                max_events: 3,
                ..test_limits(100)
            },
        )
        .await
        .expect("store");
        let change = |ordinal: u64| {
            insert_mutation(
                source_id,
                org_id,
                &plan,
                row(&format!("0010000000000{ordinal:02}AAA"), "Row", ordinal),
            )
        };
        store.commit_mutations(vec![change(1)]).await.expect("seed");
        let confirmed = Arc::new(AtomicU64::new(u64::MAX));
        let pin = store.register_subscription_pin(1, &confirmed).expect("pin");
        store
            .commit_mutations(vec![change(2), change(3), change(4)])
            .await
            .expect("retained tail");
        assert!(store.commit_mutations(vec![change(5)]).await.is_err());
        confirmed.store(0, Ordering::Release);
        assert!(
            store.commit_mutations(vec![change(5)]).await.is_err(),
            "bootstrap sentinel must protect H's suffix"
        );
        assert_eq!(
            store
                .read_events_page(2, 4, 3, 1024 * 1024)
                .await
                .expect("tail")
                .len(),
            3
        );
        confirmed.store(4, Ordering::Release);
        store
            .commit_mutations(vec![change(5), change(6), change(7)])
            .await
            .expect("confirmed history can be pruned");
        assert_eq!(
            store.earliest_resumable_ordinal().await.expect("earliest"),
            4
        );
        assert!(pin.preserves_live_history());
    }

    #[tokio::test]
    async fn replay_gap_is_detected_instead_of_silent_catchup() {
        let temp = TempDir::new().expect("temp dir");
        let source_id = "salesforce-test-source";
        let org_id = "00D000000000001AAA";
        let plan = ObjectPlan::account();
        let limits = StorageLimits {
            max_events: 3,
            ..test_limits(100)
        };
        let store = DurableMirrorStore::open(
            temp.path().to_path_buf(),
            source_id.to_string(),
            "identity-v1".to_string(),
            org_id.to_string(),
            vec!["Account".to_string()],
            limits,
        )
        .await
        .expect("open store");

        for ordinal in 1..=6u64 {
            store
                .commit_mutations(vec![insert_mutation(
                    source_id,
                    org_id,
                    &plan,
                    row(
                        &format!("0010000000001{ordinal:02}AAA"),
                        &format!("Event {ordinal}"),
                        ordinal,
                    ),
                )])
                .await
                .expect("commit");
        }

        let error = store
            .read_events_page(2, 3, 10, 1024 * 1024)
            .await
            .expect_err("missing replay range must fail closed");
        assert!(
            format!("{error:#}").contains("replay gap detected"),
            "unexpected error: {error:#}"
        );
    }

    async fn graph_store(root: &Path, limits: StorageLimits) -> Result<DurableMirrorStore> {
        DurableMirrorStore::open(
            root.to_path_buf(),
            "graph-source".into(),
            "graph-v2".into(),
            "00D000000000001AAA".into(),
            vec!["Account".into(), "Contact".into()],
            limits,
        )
        .await
    }

    fn contact(target: &str, timestamp: u64) -> GraphRecord {
        let mut contact = row("003000000000001AAA", "Contact", timestamp);
        contact.object = "Contact".into();
        contact
            .properties
            .insert("AccountId".into(), ElementValue::String(target.into()));
        contact
            .relationships
            .push(drasi_salesforce_common::LookupRelationship {
                field: "AccountId".into(),
                label: "Account".into(),
                target_object: "Account".into(),
                target_id: target.into(),
            });
        contact
    }

    #[tokio::test]
    async fn failed_record_replacement_preserves_node_edges_and_head() -> Result<()> {
        let temp = TempDir::new()?;
        let store = graph_store(
            temp.path(),
            StorageLimits {
                max_projected_row_bytes: 4096,
                ..test_limits(10)
            },
        )
        .await?;
        let original = contact("001000000000001AAA", 1000);
        let events = store
            .commit_mutations(vec![RecordMutation::Upsert(original.clone())])
            .await?;
        assert_eq!(events.len(), 2);
        let mut replacement = contact("001000000000002AAA", 2000);
        replacement
            .properties
            .insert("Name".into(), ElementValue::String("x".repeat(4096).into()));
        assert!(store
            .commit_mutations(vec![RecordMutation::Upsert(replacement)])
            .await
            .is_err());
        assert_eq!(store.head_ordinal().await?, 2);
        assert_eq!(store.load_all_rows().await?, vec![original]);
        assert_eq!(store.read_events_page(1, 2, 10, 100_000).await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_observation_preserves_graph_time_and_journal() -> Result<()> {
        let temp = TempDir::new()?;
        let store = graph_store(temp.path(), test_limits(10)).await?;
        let original = contact("001000000000001AAA", 1000);
        store
            .commit_mutations(vec![RecordMutation::Upsert(original.clone())])
            .await?;
        let mut later = original.clone();
        later.observed_at_ms = 9000;
        assert!(store
            .commit_mutations(vec![RecordMutation::Upsert(later)])
            .await?
            .is_empty());
        assert_eq!(store.head_ordinal().await?, 2);
        assert_eq!(store.load_all_rows().await?, vec![original]);
        Ok(())
    }

    #[tokio::test]
    async fn every_replacement_prefix_recovers_to_the_same_graph_snapshot() -> Result<()> {
        use drasi_core::models::Element;
        use std::collections::BTreeMap;
        fn apply(graph: &mut BTreeMap<String, Element>, change: &SourceChange) {
            match change {
                SourceChange::Insert { element } => {
                    graph.insert(
                        element.get_reference().element_id.to_string(),
                        element.clone(),
                    );
                }
                SourceChange::Update { element } => {
                    let key = element.get_reference().element_id.to_string();
                    let mut next = element.clone();
                    if let Some(previous) = graph.get(&key) {
                        next.merge_missing_properties(previous);
                    }
                    graph.insert(key, next);
                }
                SourceChange::Delete { metadata } => {
                    graph.remove(metadata.reference.element_id.as_ref());
                }
                _ => panic!("unexpected source change"),
            }
        }
        let temp = TempDir::new()?;
        let store = graph_store(temp.path(), test_limits(10)).await?;
        store
            .commit_mutations(vec![
                RecordMutation::Upsert(row("001000000000001AAA", "A", 1)),
                RecordMutation::Upsert(row("001000000000002AAA", "B", 2)),
                RecordMutation::Upsert(contact("001000000000001AAA", 3)),
            ])
            .await?;
        store
            .commit_mutations(vec![RecordMutation::Upsert(contact(
                "001000000000002AAA",
                4,
            ))])
            .await?;
        let mut renamed = contact("001000000000002AAA", 5);
        renamed
            .properties
            .insert("Name".into(), ElementValue::String("Renamed".into()));
        store
            .commit_mutations(vec![RecordMutation::Upsert(renamed)])
            .await?;
        let head = store.head_ordinal().await?;
        let original = store.read_events_page(1, head, 100, 1_000_000).await?;
        let ticket = store.create_snapshot_ticket(60).await?;
        let manifest: SnapshotManifest =
            rmp_serde::from_slice(&std::fs::read(temp.path().join(&ticket.manifest_rel_path))?)?;
        let mut reader = BufReader::new(std::fs::File::open(
            temp.path().join(manifest.rows_rel_path),
        )?);
        let mut expected = BTreeMap::new();
        while let Some(record) =
            drasi_salesforce_common::read_snapshot_record(&mut reader, 1_000_000)?
        {
            for element in std::iter::once(record.node("graph-source", "00D000000000001AAA")?)
                .chain(record.relations("graph-source", "00D000000000001AAA")?)
            {
                expected.insert(element.get_reference().element_id.to_string(), element);
            }
        }
        drop(store);
        let reopened = graph_store(temp.path(), test_limits(10)).await?;
        assert_eq!(reopened.head_ordinal().await?, head);
        for cut in 0..=head {
            let mut actual = BTreeMap::new();
            for event in original.iter().take(cut as usize) {
                let SourceEvent::Change(change) = &event.event else {
                    panic!("expected graph event")
                };
                apply(&mut actual, change);
            }
            for ordinal in cut + 1..=head {
                let page = reopened
                    .read_events_page(ordinal, head, 1, 1_000_000)
                    .await?;
                assert_eq!(page.len(), 1);
                assert_eq!(page[0].sequence, ordinal);
                let SourceEvent::Change(change) = &page[0].event else {
                    panic!("expected graph event")
                };
                apply(&mut actual, change);
            }
            assert_eq!(
                serde_json::to_value(&actual)?,
                serde_json::to_value(&expected)?,
                "cut {cut}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn schema_reprojection_rejects_stale_rows_and_resumes_progress() -> Result<()> {
        use drasi_salesforce_common::Catalog;
        use serde_json::json;
        fn schema(extra: bool) -> Result<DiscoveredSchema> {
            let mut fields = vec![
                json!({"name":"Id","type":"id","nillable":false}),
                json!({"name":"Name","type":"string","nillable":true}),
            ];
            if extra {
                fields.push(json!({"name":"Extra","type":"string","nillable":true}));
            }
            Ok(Catalog::from_describes(vec![serde_json::from_value(json!({
                "name":"Account","queryable":true,"keyPrefix":"001","fields":fields
            }))?])?
            .schema)
        }
        let temp = TempDir::new()?;
        let store = graph_store(temp.path(), test_limits(10)).await?;
        let old_schema = schema(true)?;
        store.set_schema(old_schema.clone()).await?;
        let mut original = row("001000000000001AAA", "A", 1000);
        original.schema_revision = old_schema.fingerprint;
        original
            .properties
            .insert("Extra".into(), ElementValue::String("removed".into()));
        store
            .commit_mutations(vec![RecordMutation::Upsert(original.clone())])
            .await?;
        let new_schema = schema(false)?;
        store.set_schema(new_schema.clone()).await?;
        assert_eq!(store.schema().await?.pending_records, 1);
        let error = store
            .commit_mutations(vec![RecordMutation::Upsert(original.clone())])
            .await
            .unwrap_err();
        assert!(error.is::<StaleSchema>());
        assert_eq!(store.head_ordinal().await?, 1);
        drop(store);
        let store = graph_store(temp.path(), test_limits(10)).await?;
        assert_eq!(store.schema().await?.pending_records, 1);
        original.schema_revision = new_schema.fingerprint;
        original.properties.remove("Extra");
        let changes = store
            .commit_mutations(vec![RecordMutation::Upsert(original)])
            .await?;
        let SourceEvent::Change(SourceChange::Update { element }) = &changes[0].event else {
            panic!("expected clearing node update");
        };
        assert_eq!(element.get_property("Extra"), &ElementValue::Null);
        assert_eq!(store.schema().await?.pending_records, 0);
        assert_eq!(store.record_ids("Contact").await?, Vec::<String>::new());
        assert_eq!(store.record_ids("Account").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn old_format_rejection_does_not_modify_the_old_directory() -> Result<()> {
        let temp = TempDir::new()?;
        let marker = serde_json::to_vec(&OwnershipFile {
            format_version: 1,
            source_id: "old".into(),
            source_identity: "old".into(),
        })?;
        std::fs::write(temp.path().join(OWNERSHIP_FILE), &marker)?;
        let error = graph_store(temp.path(), test_limits(10))
            .await
            .err()
            .context("v1 must fail")?;
        assert!(format!("{error:#}").contains("new v2 spool"));
        assert_eq!(std::fs::read(temp.path().join(OWNERSHIP_FILE))?, marker);
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reopen_reclaims_only_unleased_snapshot_artifacts() -> Result<()> {
        let temp = TempDir::new()?;
        let store = graph_store(temp.path(), test_limits(10)).await?;
        let ticket = store.create_snapshot_ticket(60).await?;
        let orphan = store.snapshots_dir.join(Uuid::new_v4().to_string());
        let incomplete = store.snapshots_dir.join(format!("{}.tmp", Uuid::new_v4()));
        let unrelated = store.snapshots_dir.join("notes");
        for path in [&orphan, &incomplete, &unrelated] {
            std::fs::create_dir(path)?;
        }
        drop(store);
        let _store = graph_store(temp.path(), test_limits(10)).await?;
        assert!(!orphan.exists());
        assert!(!incomplete.exists());
        assert!(unrelated.exists());
        assert!(temp.path().join(ticket.manifest_rel_path).exists());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_export_waiter_cannot_detach_work_or_race_deprovision() -> Result<()> {
        use drasi_lib::sources::Source;
        let temp = TempDir::new()?;
        let config = serde_json::from_value(serde_json::json!({
            "connection": {
                "instanceUrl": "https://example.my.salesforce.com",
                "expectedOrgId": "00D000000000001AAA",
                "auth": {
                    "type": "clientCredentials",
                    "tokenUrl": "https://example.my.salesforce.com/services/oauth2/token",
                    "clientId": "fixture", "clientSecret": "fixture"
                }
            },
            "objects": ["Account"], "spoolRoot": temp.path()
        }))?;
        let source = crate::SalesforceSource::builder("export-owner")
            .with_config(config)
            .build()?;
        let store = source.ensure_store().await?;
        let release = Arc::new(tokio::sync::Notify::new());
        let worker_release = release.clone();
        let worker_store = store.clone();
        *store.snapshot_export.lock().await = Some(tokio::spawn(async move {
            worker_release.notified().await;
            worker_store.export_snapshot(60, Utc::now().timestamp_millis() as u64)
        }));
        let waiter_store = store.clone();
        let waiter = tokio::spawn(async move { waiter_store.finish_snapshot_export().await });
        tokio::task::yield_now().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(store.has_snapshot_export().await);
        assert!(source.stop().await.is_err());
        let error = source
            .start()
            .await
            .expect_err("snapshot work must block restart");
        assert!(
            format!("{error:#}").contains("snapshot export"),
            "{error:#}"
        );
        assert!(source.deprovision().await.is_err());
        assert!(temp.path().join(DB_FILE).exists());
        release.notify_one();
        source.stop().await?;
        assert!(!store.has_snapshot_export().await);
        source.deprovision().await?;
        assert!(!temp.path().join(DB_FILE).exists());
        Ok(())
    }
}
