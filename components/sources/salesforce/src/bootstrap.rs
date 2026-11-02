use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_salesforce_common::{
    read_snapshot_record, resolve_snapshot_path, sha256_hex, SnapshotManifest, SnapshotTicket,
    MAX_SNAPSHOT_FILE_BYTES, SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY,
};
use sha2::Digest;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::spawn_blocking;

use crate::store::DurableMirrorStore;

pub(crate) struct LeasedBootstrapProvider {
    inner: Arc<dyn BootstrapProvider>,
    store: Arc<tokio::sync::RwLock<Option<Arc<DurableMirrorStore>>>>,
}

impl LeasedBootstrapProvider {
    pub fn new(
        inner: Box<dyn BootstrapProvider>,
        store: Arc<tokio::sync::RwLock<Option<Arc<DurableMirrorStore>>>>,
    ) -> Self {
        Self {
            inner: inner.into(),
            store,
        }
    }
}

#[async_trait]
impl BootstrapProvider for LeasedBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let ticket = context
            .get_typed_property::<SnapshotTicket>(SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY)?
            .context("missing Salesforce bootstrap snapshot ticket")?;
        let store = self
            .store
            .read()
            .await
            .clone()
            .context("Salesforce bootstrap store is not initialized")?;
        let inner = self.inner.clone();
        let context = context.clone();
        let settings = settings.cloned();
        store
            .run_snapshot_reader(ticket, async move {
                inner
                    .bootstrap(request, &context, event_tx, settings.as_ref())
                    .await
            })
            .await
    }
}

fn default_max_rows() -> usize {
    100_000
}

fn default_max_row_bytes() -> usize {
    1_048_576
}

const MAX_MANIFEST_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct SalesforceBootstrapProvider {
    spool_root: PathBuf,
    max_rows: usize,
    max_row_bytes: usize,
}

impl SalesforceBootstrapProvider {
    pub fn builder() -> SalesforceBootstrapProviderBuilder {
        SalesforceBootstrapProviderBuilder::new()
    }
}

pub struct SalesforceBootstrapProviderBuilder {
    spool_root: Option<PathBuf>,
    max_rows: usize,
    max_row_bytes: usize,
}

impl SalesforceBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            spool_root: None,
            max_rows: default_max_rows(),
            max_row_bytes: default_max_row_bytes(),
        }
    }

    pub fn with_spool_root(mut self, spool_root: impl Into<PathBuf>) -> Self {
        self.spool_root = Some(spool_root.into());
        self
    }

    pub fn with_max_rows(mut self, max_rows: usize) -> Self {
        self.max_rows = max_rows;
        self
    }

    pub fn with_max_row_bytes(mut self, max_row_bytes: usize) -> Self {
        self.max_row_bytes = max_row_bytes;
        self
    }

    pub fn build(self) -> Result<SalesforceBootstrapProvider> {
        let spool_root = self
            .spool_root
            .ok_or_else(|| anyhow::anyhow!("spool_root is required"))?;
        ensure!(self.max_rows > 0, "max_rows must be greater than zero");
        ensure!(
            self.max_row_bytes > 0,
            "max_row_bytes must be greater than zero"
        );
        Ok(SalesforceBootstrapProvider {
            spool_root,
            max_rows: self.max_rows,
            max_row_bytes: self.max_row_bytes,
        })
    }
}

impl Default for SalesforceBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BootstrapProvider for SalesforceBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let _ = request;

        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let ticket = context
            .get_typed_property::<SnapshotTicket>(SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY)?
            .context("missing Salesforce bootstrap snapshot ticket in context properties")?;
        ticket.validate_basic(&context.source_id, now_ms)?;

        let manifest_path = resolve_snapshot_path(&self.spool_root, &ticket.manifest_rel_path)?;
        let manifest_bytes = read_file_bounded(
            manifest_path.clone(),
            MAX_MANIFEST_BYTES.min(MAX_SNAPSHOT_FILE_BYTES as usize),
            "snapshot manifest",
        )
        .await?;
        ensure!(
            sha256_hex(&manifest_bytes).eq_ignore_ascii_case(&ticket.checksum_sha256),
            "snapshot ticket checksum mismatch"
        );
        let manifest: SnapshotManifest =
            rmp_serde::from_slice(&manifest_bytes).context("failed to decode snapshot manifest")?;
        manifest.validate_basic()?;
        ensure!(
            manifest.row_count <= self.max_rows as u64,
            "snapshot row count {} exceeds configured max_rows {}",
            manifest.row_count,
            self.max_rows
        );
        ensure!(
            same_object_set(&manifest.objects, &ticket.objects),
            "snapshot manifest objects do not match ticket objects"
        );
        ensure!(
            manifest.source_id == ticket.source_id
                && manifest.source_identity == ticket.source_identity
                && manifest.epoch == ticket.epoch
                && manifest.head_ordinal == ticket.head_ordinal
                && manifest.org_id == ticket.org_id,
            "snapshot manifest does not match ticket identity"
        );

        let rows_path = resolve_snapshot_path(&self.spool_root, &manifest.rows_rel_path)?;
        let rows_file_limit = rows_file_read_limit(manifest.row_count, self.max_row_bytes);
        let rows_hash =
            sha256_file_bounded(rows_path.clone(), rows_file_limit, "snapshot rows").await?;
        ensure!(
            rows_hash.eq_ignore_ascii_case(&manifest.rows_sha256),
            "snapshot rows checksum mismatch"
        );

        let selected_objects = manifest
            .objects
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        let prevalidated_rows = prevalidate_snapshot_rows(
            rows_path.clone(),
            self.max_row_bytes,
            selected_objects.clone(),
        )
        .await?;
        ensure!(
            prevalidated_rows == manifest.row_count,
            "snapshot row count mismatch after prevalidation (manifest={}, parsed={})",
            manifest.row_count,
            prevalidated_rows
        );

        let source_id = context.source_id.clone();
        let org_id = manifest.org_id.clone();
        let node_pass = stream_snapshot_pass(SnapshotPassConfig {
            rows_path: rows_path.clone(),
            max_record_bytes: self.max_row_bytes,
            selected_objects: selected_objects.clone(),
            pass: SnapshotPass::Nodes,
            source_id: source_id.clone(),
            org_id: org_id.clone(),
            context: context.clone(),
            event_tx: event_tx.clone(),
        })
        .await?;
        ensure!(
            node_pass.rows == manifest.row_count,
            "snapshot row count mismatch (manifest={}, parsed={})",
            manifest.row_count,
            node_pass.rows
        );

        let relation_pass = stream_snapshot_pass(SnapshotPassConfig {
            rows_path,
            max_record_bytes: self.max_row_bytes,
            selected_objects,
            pass: SnapshotPass::Relations,
            source_id,
            org_id,
            context: context.clone(),
            event_tx,
        })
        .await?;
        ensure!(
            relation_pass.rows == manifest.row_count,
            "snapshot row count mismatch during relation pass (manifest={}, parsed={})",
            manifest.row_count,
            relation_pass.rows
        );

        Ok(BootstrapResult {
            event_count: node_pass.events.saturating_add(relation_pass.events),
            source_position: Some(manifest.local_position()),
        })
    }
}

#[derive(Clone, Copy)]
enum SnapshotPass {
    Nodes,
    Relations,
}

struct PassStats {
    rows: u64,
    events: usize,
}

struct SnapshotPassConfig {
    rows_path: PathBuf,
    max_record_bytes: usize,
    selected_objects: BTreeSet<String>,
    pass: SnapshotPass,
    source_id: String,
    org_id: String,
    context: BootstrapContext,
    event_tx: BootstrapEventSender,
}

async fn stream_snapshot_pass(config: SnapshotPassConfig) -> Result<PassStats> {
    let SnapshotPassConfig {
        rows_path,
        max_record_bytes,
        selected_objects,
        pass,
        source_id,
        org_id,
        context,
        event_tx,
    } = config;
    spawn_blocking(move || -> Result<PassStats> {
        let file = std::fs::File::open(&rows_path)
            .with_context(|| format!("failed to open snapshot rows {}", rows_path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut rows = 0u64;
        let mut events = 0usize;

        loop {
            let Some(record) = read_snapshot_record(&mut reader, max_record_bytes)? else {
                break;
            };
            rows = rows.saturating_add(1);
            match pass {
                SnapshotPass::Nodes => {
                    let element = record.node(&source_id, &org_id)?;
                    event_tx
                        .blocking_send(BootstrapEvent {
                            source_id: source_id.clone(),
                            change: SourceChange::Insert { element },
                            timestamp: chrono::Utc::now(),
                            sequence: context.next_sequence(),
                        })
                        .map_err(|_| {
                            anyhow::anyhow!("failed to deliver Salesforce bootstrap node event")
                        })?;
                    events = events.saturating_add(1);
                }
                SnapshotPass::Relations => {
                    for element in record.relations(&source_id, &org_id)? {
                        event_tx
                            .blocking_send(BootstrapEvent {
                                source_id: source_id.clone(),
                                change: SourceChange::Insert { element },
                                timestamp: chrono::Utc::now(),
                                sequence: context.next_sequence(),
                            })
                            .map_err(|_| {
                                anyhow::anyhow!(
                                    "failed to deliver Salesforce bootstrap relation event"
                                )
                            })?;
                        events = events.saturating_add(1);
                    }
                }
            }
        }

        Ok(PassStats { rows, events })
    })
    .await?
}

async fn prevalidate_snapshot_rows(
    rows_path: PathBuf,
    max_record_bytes: usize,
    selected_objects: BTreeSet<String>,
) -> Result<u64> {
    spawn_blocking(move || -> Result<u64> {
        let file = std::fs::File::open(&rows_path)
            .with_context(|| format!("failed to open snapshot rows {}", rows_path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        let mut rows = 0u64;

        loop {
            let Some(record) = read_snapshot_record(&mut reader, max_record_bytes)? else {
                break;
            };
            rows = rows.saturating_add(1);
            record.validate()?;
            ensure!(
                selected_objects.contains(&record.object),
                "snapshot contains unexpected object '{}'",
                record.object
            );
            for relationship in &record.relationships {
                ensure!(
                    selected_objects.contains(&relationship.target_object),
                    "snapshot relationship '{}' from '{}' targets out-of-scope object '{}'",
                    relationship.field,
                    record.object,
                    relationship.target_object
                );
            }
        }

        Ok(rows)
    })
    .await?
}

fn same_object_set(left: &[String], right: &[String]) -> bool {
    left.iter().collect::<BTreeSet<_>>() == right.iter().collect::<BTreeSet<_>>()
}

fn rows_file_read_limit(row_count: u64, max_row_bytes: usize) -> usize {
    let rows = usize::try_from(row_count).unwrap_or(usize::MAX);
    let per_row_with_overhead = max_row_bytes.saturating_add(1024);
    let derived = rows
        .saturating_mul(per_row_with_overhead)
        .saturating_add(1024 * 1024);
    derived.min(MAX_SNAPSHOT_FILE_BYTES as usize)
}

async fn read_file_bounded(path: PathBuf, max_bytes: usize, label: &str) -> Result<Vec<u8>> {
    ensure!(
        max_bytes > 0,
        "{label} max read size must be greater than zero"
    );
    let label = label.to_string();
    spawn_blocking(move || -> Result<Vec<u8>> {
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat {} {}", label, path.display()))?;
        ensure!(
            metadata.is_file(),
            "{} path is not a regular file: {}",
            label,
            path.display()
        );
        let declared_len = usize::try_from(metadata.len()).with_context(|| {
            format!(
                "{} size does not fit in usize ({} bytes): {}",
                label,
                metadata.len(),
                path.display()
            )
        })?;
        ensure!(
            declared_len <= max_bytes,
            "{} {} bytes exceeds read limit {} bytes: {}",
            label,
            declared_len,
            max_bytes,
            path.display()
        );

        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open {} {}", label, path.display()))?;
        let mut bytes = Vec::with_capacity(declared_len);
        file.take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {} {}", label, path.display()))?;
        ensure!(
            bytes.len() <= max_bytes,
            "{} actual size exceeds read limit {} bytes: {}",
            label,
            max_bytes,
            path.display()
        );
        Ok(bytes)
    })
    .await?
}

async fn sha256_file_bounded(path: PathBuf, max_bytes: usize, label: &str) -> Result<String> {
    ensure!(
        max_bytes > 0,
        "{label} max read size must be greater than zero"
    );
    let label = label.to_string();
    spawn_blocking(move || -> Result<String> {
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("failed to stat {} {}", label, path.display()))?;
        ensure!(
            metadata.is_file(),
            "{} path is not a regular file: {}",
            label,
            path.display()
        );
        ensure!(
            metadata.len() <= max_bytes as u64,
            "{} {} bytes exceeds read limit {} bytes: {}",
            label,
            metadata.len(),
            max_bytes,
            path.display()
        );

        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open {} {}", label, path.display()))?;
        let mut hasher = sha2::Sha256::new();
        let mut total = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .with_context(|| format!("failed to read {} {}", label, path.display()))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read);
            ensure!(
                total <= max_bytes,
                "{} actual size exceeds read limit {} bytes: {}",
                label,
                max_bytes,
                path.display()
            );
            use sha2::Digest;
            hasher.update(&buffer[..read]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::Element;
    use drasi_lib::bootstrap::BootstrapContext;
    use drasi_salesforce_common::{
        write_snapshot_record, GraphRecord, LocalPosition, LookupRelationship, Properties,
    };
    use std::collections::{BTreeMap, HashMap};
    use tempfile::TempDir;

    const SOURCE_ID: &str = "salesforce-source";
    const ORG_ID: &str = "00D000000000001AAA";

    fn sample_request() -> BootstrapRequest {
        BootstrapRequest {
            query_id: "q1".to_string(),
            node_labels: vec!["Contact".to_string()],
            relation_labels: Vec::new(),
            request_id: "req-1".to_string(),
        }
    }

    fn account_record() -> GraphRecord {
        GraphRecord {
            object: "Account".to_string(),
            id: "001000000000001AAA".to_string(),
            properties: BTreeMap::from([(
                "Name".to_string(),
                drasi_core::models::ElementValue::String("Acme".into()),
            )]),
            relationships: Vec::new(),
            observed_at_ms: 1_700_000_000_010,
            schema_revision: "schema-v2".to_string(),
        }
    }

    fn contact_record() -> GraphRecord {
        GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([(
                "Name".to_string(),
                drasi_core::models::ElementValue::String("Ada".into()),
            )]),
            relationships: vec![LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 1_700_000_000_020,
            schema_revision: "schema-v2".to_string(),
        }
    }

    fn write_snapshot(
        spool_root: &std::path::Path,
        ticket_id: &str,
        epoch: u128,
    ) -> Result<SnapshotTicket> {
        write_snapshot_with_records(
            spool_root,
            ticket_id,
            epoch,
            vec![contact_record(), account_record()],
        )
    }

    fn write_snapshot_with_records(
        spool_root: &std::path::Path,
        ticket_id: &str,
        epoch: u128,
        records: Vec<GraphRecord>,
    ) -> Result<SnapshotTicket> {
        let snapshot_dir = spool_root.join("snapshots").join(ticket_id);
        std::fs::create_dir_all(&snapshot_dir)?;

        let mut rows_bytes = Vec::new();
        for record in &records {
            write_snapshot_record(&mut rows_bytes, record, 1_048_576)?;
        }
        std::fs::write(snapshot_dir.join("rows.msgpack"), &rows_bytes)?;

        let manifest = SnapshotManifest {
            version: drasi_salesforce_common::SNAPSHOT_MANIFEST_VERSION,
            source_id: SOURCE_ID.to_string(),
            source_identity: "identity-v2".to_string(),
            org_id: ORG_ID.to_string(),
            objects: vec!["Account".to_string(), "Contact".to_string()],
            epoch,
            head_ordinal: 7,
            created_at_ms: 1_700_000_100_000,
            rows_rel_path: format!("snapshots/{ticket_id}/rows.msgpack"),
            row_count: records.len() as u64,
            rows_sha256: sha256_hex(&rows_bytes),
        };
        let manifest_bytes = rmp_serde::to_vec_named(&manifest)?;
        std::fs::write(snapshot_dir.join("manifest.msgpack"), &manifest_bytes)?;

        Ok(SnapshotTicket {
            version: drasi_salesforce_common::SNAPSHOT_MANIFEST_VERSION,
            ticket_id: ticket_id.to_string(),
            source_id: SOURCE_ID.to_string(),
            source_identity: "identity-v2".to_string(),
            org_id: ORG_ID.to_string(),
            objects: vec!["Account".to_string(), "Contact".to_string()],
            epoch: manifest.epoch,
            head_ordinal: manifest.head_ordinal,
            manifest_rel_path: format!("snapshots/{ticket_id}/manifest.msgpack"),
            expires_at_ms: chrono::Utc::now().timestamp_millis() as u64 + 60_000,
            checksum_sha256: sha256_hex(&manifest_bytes),
        })
    }

    #[tokio::test]
    async fn bootstrap_reads_snapshot_and_returns_source_position() -> Result<()> {
        let temp = TempDir::new()?;
        let ticket = write_snapshot(temp.path(), "ticket-1", 42)?;

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;

        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let result = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await?;

        assert_eq!(result.event_count, 3);
        assert_eq!(result.source_position, Some(ticket.local_position()));

        let first = rx.recv().await.expect("first bootstrap event");
        let second = rx.recv().await.expect("second bootstrap event");
        let third = rx.recv().await.expect("third bootstrap event");
        assert!(
            matches!(
                first.change,
                SourceChange::Insert {
                    element: Element::Node { .. }
                }
            ) && matches!(
                second.change,
                SourceChange::Insert {
                    element: Element::Node { .. }
                }
            ) && matches!(
                third.change,
                SourceChange::Insert {
                    element: Element::Relation { .. }
                }
            ),
            "bootstrap must emit all nodes before any relations"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_keeps_unlabeled_endpoints_when_query_labels_are_sparse() -> Result<()> {
        let temp = TempDir::new()?;
        let ticket = write_snapshot(temp.path(), "ticket-2", 42)?;

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;
        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let result = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await?;
        assert_eq!(result.event_count, 3);
        let mut saw_account_node = false;
        let mut saw_contact_node = false;
        let mut saw_relation = false;
        while let Some(event) = rx.recv().await {
            match event.change {
                SourceChange::Insert {
                    element: Element::Node { ref metadata, .. },
                } => {
                    if metadata
                        .labels
                        .iter()
                        .any(|label| label.as_ref() == "Account")
                    {
                        saw_account_node = true;
                    }
                    if metadata
                        .labels
                        .iter()
                        .any(|label| label.as_ref() == "Contact")
                    {
                        saw_contact_node = true;
                    }
                }
                SourceChange::Insert {
                    element: Element::Relation { .. },
                } => saw_relation = true,
                _ => {}
            }
            if saw_account_node && saw_contact_node && saw_relation {
                break;
            }
        }
        assert!(saw_account_node && saw_contact_node && saw_relation);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_rejects_duplicate_lookup_slots_before_emitting_events() -> Result<()> {
        let temp = TempDir::new()?;
        let mut duplicate = contact_record();
        duplicate.relationships.push(LookupRelationship {
            field: "AccountId".to_string(),
            label: "Account".to_string(),
            target_object: "Account".to_string(),
            target_id: "001000000000001AAA".to_string(),
        });
        let ticket =
            write_snapshot_with_records(temp.path(), "ticket-dup-lookup", 42, vec![duplicate])?;

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;
        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        let error = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await
            .expect_err("duplicate lookup slots must fail prevalidation");
        assert!(
            format!("{error:#}").contains("duplicate relationship field"),
            "unexpected error: {error:#}"
        );
        assert!(
            rx.recv().await.is_none(),
            "bootstrap emitted events before duplicate-slot prevalidation failed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_rejects_out_of_scope_lookup_targets_before_emitting_events() -> Result<()> {
        let temp = TempDir::new()?;
        let mut out_of_scope = contact_record();
        out_of_scope.relationships = vec![LookupRelationship {
            field: "OwnerId".to_string(),
            label: "Owner".to_string(),
            target_object: "User".to_string(),
            target_id: "005000000000001AAA".to_string(),
        }];
        let ticket = write_snapshot_with_records(
            temp.path(),
            "ticket-out-of-scope-target",
            42,
            vec![out_of_scope],
        )?;

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;
        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        let error = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await
            .expect_err("out-of-scope lookup target must fail prevalidation");
        assert!(
            format!("{error:#}").contains("out-of-scope object"),
            "unexpected error: {error:#}"
        );
        assert!(
            rx.recv().await.is_none(),
            "bootstrap emitted events before out-of-scope-target prevalidation failed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_rejects_ticket_checksum_mismatch() -> Result<()> {
        let temp = TempDir::new()?;
        let mut ticket = write_snapshot(temp.path(), "ticket-3", 42)?;
        ticket.checksum_sha256 = "0".repeat(64);

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;
        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let error = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await
            .expect_err("checksum mismatch should fail");
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_round_trips_high_bit_epoch_through_context_codec() -> Result<()> {
        let temp = TempDir::new()?;
        let high_epoch = (1u128 << 127) + 123;
        let ticket = write_snapshot(temp.path(), "ticket-high-epoch", high_epoch)?;

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;

        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );

        let decoded = context
            .get_typed_property::<SnapshotTicket>(SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY)?
            .expect("typed ticket");
        assert_eq!(decoded.epoch, high_epoch);

        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let result = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await?;
        let position = result.source_position.expect("bootstrap source_position");
        let local = LocalPosition::decode(&position)?;
        assert_eq!(local.epoch, high_epoch);
        assert_eq!(local.ordinal, ticket.head_ordinal);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_rejects_oversized_manifest_before_decode() -> Result<()> {
        let temp = TempDir::new()?;
        let ticket_id = "ticket-oversized-manifest";
        let snapshot_dir = temp.path().join("snapshots").join(ticket_id);
        std::fs::create_dir_all(&snapshot_dir)?;
        std::fs::write(snapshot_dir.join("rows.msgpack"), vec![0u8; 16])?;

        let oversized_manifest = vec![b'x'; MAX_MANIFEST_BYTES + 1];
        std::fs::write(snapshot_dir.join("manifest.msgpack"), &oversized_manifest)?;

        let ticket = SnapshotTicket {
            version: drasi_salesforce_common::SNAPSHOT_MANIFEST_VERSION,
            ticket_id: ticket_id.to_string(),
            source_id: SOURCE_ID.to_string(),
            source_identity: "identity-v2".to_string(),
            org_id: ORG_ID.to_string(),
            objects: vec!["Account".to_string()],
            epoch: 42,
            head_ordinal: 7,
            manifest_rel_path: format!("snapshots/{ticket_id}/manifest.msgpack"),
            expires_at_ms: chrono::Utc::now().timestamp_millis() as u64 + 60_000,
            checksum_sha256: sha256_hex(&oversized_manifest),
        };

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .build()?;
        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let error = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await
            .expect_err("oversized manifest should fail before decode");
        assert!(
            format!("{error:#}").contains("exceeds read limit"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_rejects_oversized_rows_file_before_decode() -> Result<()> {
        let temp = TempDir::new()?;
        let ticket_id = "ticket-oversized-rows";
        let snapshot_dir = temp.path().join("snapshots").join(ticket_id);
        std::fs::create_dir_all(&snapshot_dir)?;

        let rows_limit = rows_file_read_limit(1, 128);
        let oversized_rows = vec![b'y'; rows_limit + 1];
        std::fs::write(snapshot_dir.join("rows.msgpack"), &oversized_rows)?;

        let manifest = SnapshotManifest {
            version: drasi_salesforce_common::SNAPSHOT_MANIFEST_VERSION,
            source_id: SOURCE_ID.to_string(),
            source_identity: "identity-v2".to_string(),
            org_id: ORG_ID.to_string(),
            objects: vec!["Account".to_string()],
            epoch: 42,
            head_ordinal: 7,
            created_at_ms: 1_700_000_100_000,
            rows_rel_path: format!("snapshots/{ticket_id}/rows.msgpack"),
            row_count: 1,
            rows_sha256: sha256_hex(&oversized_rows),
        };
        let manifest_bytes = rmp_serde::to_vec_named(&manifest)?;
        std::fs::write(snapshot_dir.join("manifest.msgpack"), &manifest_bytes)?;

        let ticket = SnapshotTicket {
            version: drasi_salesforce_common::SNAPSHOT_MANIFEST_VERSION,
            ticket_id: ticket_id.to_string(),
            source_id: SOURCE_ID.to_string(),
            source_identity: "identity-v2".to_string(),
            org_id: ORG_ID.to_string(),
            objects: vec!["Account".to_string()],
            epoch: 42,
            head_ordinal: 7,
            manifest_rel_path: format!("snapshots/{ticket_id}/manifest.msgpack"),
            expires_at_ms: chrono::Utc::now().timestamp_millis() as u64 + 60_000,
            checksum_sha256: sha256_hex(&manifest_bytes),
        };

        let provider = SalesforceBootstrapProvider::builder()
            .with_spool_root(temp.path())
            .with_max_row_bytes(128)
            .build()?;
        let mut properties = HashMap::new();
        properties.insert(
            SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY.to_string(),
            serde_json::to_value(&ticket)?,
        );
        let context = BootstrapContext::with_properties(
            "server-1".to_string(),
            SOURCE_ID.to_string(),
            properties,
        );
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let error = provider
            .bootstrap(sample_request(), &context, tx, None)
            .await
            .expect_err("oversized rows should fail before decode");
        assert!(
            format!("{error:#}").contains("exceeds read limit"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }
}
