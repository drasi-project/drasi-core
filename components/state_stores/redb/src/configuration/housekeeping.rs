// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{num::NonZeroUsize, ops::Bound};

use super::*;

const POLICY: &str = "request-policy";
const REQUEST_PREFIX: &str = "drasi-config-v1";

/// Opt-in receipt expiry. Full batches expire when a new request ID is generated.
/// Omission retains arbitrary request IDs and their receipts indefinitely.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationStoreOptions {
    pub receipt_batch_capacity: Option<NonZeroUsize>,
}

fn metadata_context(name: &str) -> String {
    if name == "key-check" {
        "drasi.configuration.key-check".into()
    } else {
        format!("metadata:{name}")
    }
}

pub(super) fn initialize_policy(
    owner: &DatabaseOwner,
    transaction: &redb::WriteTransaction,
    version: u32,
) -> Result<()> {
    let mut metadata = transaction.open_table(METADATA)?;
    let previous: ConfigurationStoreOptions = metadata
        .get(POLICY)?
        .map(|value| unseal(owner, &metadata_context(POLICY), value.value()))
        .transpose()?
        .unwrap_or_default();
    anyhow::ensure!(
        (version == 2) == previous.receipt_batch_capacity.is_some(),
        "configuration format and receipt expiry policy disagree"
    );
    if previous.receipt_batch_capacity.is_some() {
        anyhow::ensure!(
            previous == owner.options,
            "persisted receipt expiry cannot be disabled or silently reconfigured"
        );
    } else if owner.options.receipt_batch_capacity.is_some() {
        // A format boundary prevents an older binary from reopening expired IDs
        // under the old indefinite-retry contract.
        transaction.open_table(REQUESTS)?.retain(|_, _| false)?;
        let bytes = seal(owner, &metadata_context("key-check"), &2u32)?;
        metadata.insert("key-check", bytes.as_slice())?;
    }
    if previous != owner.options || metadata.get(POLICY)?.is_none() {
        let bytes = seal(owner, &metadata_context(POLICY), &owner.options)?;
        metadata.insert(POLICY, bytes.as_slice())?;
    }
    Ok(())
}

fn instance_prefix(instance: &str) -> Result<String> {
    Ok(format!("[{},", serde_json::to_string(instance)?))
}

fn window_key(instance: &str) -> String {
    format!("request-window:{instance}")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestWindow {
    namespace: Uuid,
    batch: u64,
    accepted: usize,
}

impl RequestWindow {
    fn validate(&self, capacity: NonZeroUsize) -> Result<()> {
        anyhow::ensure!(
            !self.namespace.is_nil() && self.batch > 0 && self.accepted <= capacity.get(),
            "invalid configuration request batch"
        );
        Ok(())
    }

    fn check(&self, id: &str) -> Result<()> {
        let parts: Vec<_> = id.split('/').collect();
        if parts.len() != 4 || parts[0] != REQUEST_PREFIX {
            return Err(ManagementError::GeneratedRequestIdRequired.into());
        }
        let namespace =
            Uuid::parse_str(parts[1]).map_err(|_| ManagementError::GeneratedRequestIdRequired)?;
        let batch = parts[2]
            .parse::<u64>()
            .map_err(|_| ManagementError::GeneratedRequestIdRequired)?;
        let request =
            Uuid::parse_str(parts[3]).map_err(|_| ManagementError::GeneratedRequestIdRequired)?;
        if namespace.to_string() != parts[1]
            || batch.to_string() != parts[2]
            || request.is_nil()
            || request.to_string() != parts[3]
        {
            return Err(ManagementError::GeneratedRequestIdRequired.into());
        }
        if namespace != self.namespace || batch != self.batch {
            return Err(ManagementError::RequestExpired.into());
        }
        Ok(())
    }

    pub(super) fn accept(&mut self, options: ConfigurationStoreOptions) -> Result<()> {
        let capacity = options
            .receipt_batch_capacity
            .context("missing receipt batch policy")?;
        if self.accepted >= capacity.get() {
            return Err(ManagementError::RequestBatchFull.into());
        }
        self.accepted += 1;
        Ok(())
    }

    pub(super) fn save(&self, lease: &Lease, transaction: &redb::WriteTransaction) -> Result<()> {
        let name = window_key(&lease.instance);
        let bytes = seal(&lease.owner, &metadata_context(&name), self)?;
        transaction
            .open_table(METADATA)?
            .insert(name.as_str(), bytes.as_slice())?;
        Ok(())
    }
}

fn read_window(
    lease: &Lease,
    metadata: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Option<RequestWindow>> {
    let name = window_key(&lease.instance);
    let window: Option<RequestWindow> = metadata
        .get(name.as_str())?
        .map(|value| unseal(&lease.owner, &metadata_context(&name), value.value()))
        .transpose()?;
    if let Some(window) = &window {
        window.validate(
            lease
                .owner
                .options
                .receipt_batch_capacity
                .context("missing receipt batch policy")?,
        )?;
    }
    Ok(window)
}

pub(super) fn check_request(
    lease: &Lease,
    metadata: &impl ReadableTable<&'static str, &'static [u8]>,
    request: &str,
) -> Result<Option<RequestWindow>> {
    if lease.owner.options.receipt_batch_capacity.is_none() {
        return Ok(None);
    }
    let window =
        read_window(lease, metadata)?.ok_or(ManagementError::GeneratedRequestIdRequired)?;
    window.check(request)?;
    Ok(Some(window))
}

pub(super) fn validate_window(lease: &Lease) -> Result<()> {
    let transaction = lease.owner.database.begin_read()?;
    let window = read_window(lease, &transaction.open_table(METADATA)?)?;
    let current = current(
        &lease.owner,
        &transaction.open_table(CURRENT)?,
        &lease.instance,
    )?;
    let prefix = instance_prefix(&lease.instance)?;
    let requests = transaction.open_table(REQUESTS)?;
    let mut count = 0usize;
    for entry in requests.range(prefix.as_str()..)? {
        let (name, value) = entry?;
        if !name.value().starts_with(&prefix) {
            break;
        }
        let (instance, id): (String, String) = serde_json::from_str(name.value())?;
        anyhow::ensure!(
            instance == lease.instance,
            "configuration request namespace mismatch"
        );
        let window = window
            .as_ref()
            .context("configuration receipts have no request batch")?;
        window.check(&id)?;
        let record: RequestRecord = unseal(
            &lease.owner,
            &format!("request:{}", name.value()),
            value.value(),
        )?;
        anyhow::ensure!(
            record.receipt.request_id == id
                && record.receipt.durable
                && record.receipt.revision <= current.revision
                && (record.receipt.revision != current.revision
                    || record.desired == current.desired),
            "configuration receipt disagrees with committed state"
        );
        count = count
            .checked_add(1)
            .context("configuration receipt count overflow")?;
        anyhow::ensure!(
            count <= window.accepted,
            "configuration receipt count exceeds its batch"
        );
    }
    anyhow::ensure!(
        count == window.as_ref().map_or(0, |window| window.accepted),
        "configuration receipt batch is incomplete"
    );
    Ok(())
}

pub(super) fn new_request_id(lease: &Lease) -> Result<String> {
    let Some(capacity) = lease.owner.options.receipt_batch_capacity else {
        return Ok(Uuid::new_v4().to_string());
    };
    let mut transaction = lease.owner.database.begin_write()?;
    transaction.set_durability(Durability::Immediate);
    let saved = read_window(lease, &transaction.open_table(METADATA)?)?;
    let mut changed = saved.is_none();
    let mut window = saved.unwrap_or_else(|| RequestWindow {
        namespace: Uuid::new_v4(),
        batch: 1,
        accepted: 0,
    });
    if window.accepted == capacity.get() {
        window.batch = window
            .batch
            .checked_add(1)
            .context("configuration request batches exhausted")?;
        window.accepted = 0;
        let prefix = instance_prefix(&lease.instance)?;
        let mut requests = transaction.open_table(REQUESTS)?;
        loop {
            let name = {
                let first = requests.range(prefix.as_str()..)?.next().transpose()?;
                first.and_then(|(key, _)| {
                    key.value()
                        .starts_with(&prefix)
                        .then(|| key.value().to_owned())
                })
            };
            let Some(name) = name else {
                break;
            };
            requests.remove(name.as_str())?;
        }
        changed = true;
    }
    let id = format!(
        "{REQUEST_PREFIX}/{}/{}/{}",
        window.namespace,
        window.batch,
        Uuid::new_v4()
    );
    if changed {
        window.save(lease, &transaction)?;
        transaction.commit().context(
            "request batch transition could not be confirmed; reload before using a new ID",
        )?;
    }
    Ok(id)
}

pub(super) fn list_snapshots(
    lease: &Lease,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Vec<ConfigurationSnapshotSummary>> {
    let prefix = instance_prefix(&lease.instance)?;
    let start = after
        .map(|after| key(&lease.instance, after))
        .transpose()?
        .unwrap_or_else(|| prefix.clone());
    let lower = if after.is_some() {
        Bound::Excluded(start.as_str())
    } else {
        Bound::Included(start.as_str())
    };
    let transaction = lease.owner.database.begin_read()?;
    let snapshots = transaction.open_table(SNAPSHOTS)?;
    let mut result = Vec::new();
    for row in snapshots.range::<&str>((lower, Bound::Unbounded))? {
        let (name, value) = row?;
        if !name.value().starts_with(&prefix) {
            break;
        }
        let (instance, snapshot_name): (String, String) = serde_json::from_str(name.value())?;
        anyhow::ensure!(
            instance == lease.instance,
            "configuration snapshot namespace mismatch"
        );
        let snapshot: CommittedConfiguration = unseal(
            &lease.owner,
            &format!("snapshot:{}", name.value()),
            value.value(),
        )?;
        result.push(ConfigurationSnapshotSummary {
            name: snapshot_name,
            revision: snapshot.revision,
        });
        if result.len() == limit.get() {
            break;
        }
    }
    Ok(result)
}

pub(super) fn delete_snapshot(lease: &Lease, name: &str, expected: u64) -> Result<bool> {
    let record_key = key(&lease.instance, name)?;
    let mut transaction = lease.owner.database.begin_write()?;
    transaction.set_durability(Durability::Immediate);
    {
        let mut snapshots = transaction.open_table(SNAPSHOTS)?;
        let saved: Option<CommittedConfiguration> = snapshots
            .get(record_key.as_str())?
            .map(|value| {
                unseal(
                    &lease.owner,
                    &format!("snapshot:{record_key}"),
                    value.value(),
                )
            })
            .transpose()?;
        let Some(saved) = saved else {
            return Ok(false);
        };
        if saved.revision != expected {
            return Err(ManagementError::RevisionConflict {
                expected,
                actual: saved.revision,
            }
            .into());
        }
        snapshots.remove(record_key.as_str())?;
    }
    transaction
        .commit()
        .context("snapshot deletion could not be confirmed; reload the named snapshot")?;
    Ok(true)
}

struct RotationGuard(Arc<DatabaseOwner>);

impl Drop for RotationGuard {
    fn drop(&mut self) {
        self.0.rotating.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
enum RotationPhase {
    BeforeCommit,
    AfterCommit,
}

impl RedbConfigurationStore {
    /// Re-encrypt configuration, receipts, snapshots and metadata atomically.
    /// All sessions must be closed. Provision the replacement key externally
    /// first; retain both keys until success, or confirm which key opens the
    /// database after an uncertain result. This does not rotate processing state.
    pub async fn rotate_key(&self, key: [u8; 32]) -> Result<()> {
        self.rotate_key_with(key, |_| {}).await
    }

    async fn rotate_key_with(
        &self,
        key: [u8; 32],
        phase: impl Fn(RotationPhase) + Send + 'static,
    ) -> Result<()> {
        let owner = self.owner.clone();
        {
            let leases = owner
                .leases
                .lock()
                .map_err(|_| anyhow::anyhow!("configuration ownership lock poisoned"))?;
            anyhow::ensure!(
                leases.is_empty(),
                "close every configuration session before rotating its database key"
            );
            anyhow::ensure!(
                !owner.key_unconfirmed.load(Ordering::Acquire),
                "configuration encryption key is unconfirmed; reopen the database"
            );
            anyhow::ensure!(
                !owner.rotating.swap(true, Ordering::AcqRel),
                "configuration key rotation is already in progress"
            );
        }
        let rotation = RotationGuard(owner);
        tokio::task::spawn_blocking(move || {
            let owner = &rotation.0;
            let mut cipher = owner
                .cipher
                .write()
                .map_err(|_| anyhow::anyhow!("configuration encryption lock poisoned"))?;
            let replacement = XChaCha20Poly1305::new((&key).into());
            let mut transaction = owner.database.begin_write()?;
            transaction.set_durability(Durability::Immediate);
            for (definition, prefix) in [
                (CURRENT, "current"),
                (REQUESTS, "request"),
                (SNAPSHOTS, "snapshot"),
                (METADATA, "metadata"),
            ] {
                let mut table = transaction.open_table(definition)?;
                let mut after: Option<String> = None;
                loop {
                    let next = {
                        let lower = after.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
                        let row = table
                            .range::<&str>((lower, Bound::Unbounded))?
                            .next()
                            .transpose()?;
                        row.map(|(name, value)| {
                            let context = if prefix == "metadata" {
                                metadata_context(name.value())
                            } else {
                                format!("{prefix}:{}", name.value())
                            };
                            let plaintext = unseal_bytes(&cipher, &context, value.value())?;
                            let bytes = seal_bytes(&replacement, &context, &plaintext)?;
                            Ok::<_, anyhow::Error>((name.value().to_owned(), bytes))
                        })
                        .transpose()?
                    };
                    let Some((name, bytes)) = next else {
                        break;
                    };
                    table.insert(name.as_str(), bytes.as_slice())?;
                    after = Some(name);
                }
            }
            phase(RotationPhase::BeforeCommit);
            owner.key_unconfirmed.store(true, Ordering::Release);
            transaction.commit().context(
                "key rotation outcome is unconfirmed; reopen with the original or replacement key",
            )?;
            phase(RotationPhase::AfterCommit);
            *cipher = replacement;
            owner.key_unconfirmed.store(false, Ordering::Release);
            Ok(())
        })
        .await
        .context("configuration key rotation worker failed")?
    }
}

#[cfg(test)]
mod tests;
