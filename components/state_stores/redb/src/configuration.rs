// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    XChaCha20Poly1305, XNonce,
};
use drasi_lib::management::{
    AcceptanceReceipt, AcceptedComponentCreation, AcceptedComponentUpdate, CommittedConfiguration,
    ComponentUpdateCompletion, ConfigurationSession, ConfigurationSnapshotSummary,
    ConfigurationStore, ConfigurationStoreGuarantees, DesiredInstance, ManagementError,
};
use redb::{Database, Durability, ReadableTable, TableDefinition};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uuid::Uuid;

const CURRENT: TableDefinition<&str, &[u8]> = TableDefinition::new("drasi.configuration.current");
const REQUESTS: TableDefinition<&str, &[u8]> = TableDefinition::new("drasi.configuration.requests");
const SNAPSHOTS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("drasi.configuration.snapshots");
const METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("drasi.configuration.metadata");
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const UUID_CONFIGURATION_FORMAT: u32 = 7;
const UUID_EXPIRING_CONFIGURATION_FORMAT: u32 = 8;

mod housekeeping;
pub use housekeeping::ConfigurationStoreOptions;
#[cfg(test)]
mod reliability_tests;
#[cfg(test)]
mod update_tests;

struct DatabaseOwner {
    database: Database,
    cipher: RwLock<XChaCha20Poly1305>,
    leases: Mutex<BTreeMap<String, Uuid>>,
    options: ConfigurationStoreOptions,
    rotating: AtomicBool,
    key_unconfirmed: AtomicBool,
}

/// Encrypted authoritative graph configuration, separate from processing state.
/// Clone one provider to host multiple instance IDs in one redb database. redb
/// excludes other processes from opening the database concurrently.
///
/// The 32-byte key must come from outside this database. Keep it available across
/// restart; losing it makes stored definitions, receipts and snapshots unreadable.
/// Record keys (instance/request/snapshot names) are not encrypted.
#[derive(Clone)]
pub struct RedbConfigurationStore {
    owner: Arc<DatabaseOwner>,
}

impl RedbConfigurationStore {
    /// IDs with an accepted configuration, including accepted empty definitions.
    /// Used for identity discovery before a host opens an instance session.
    pub async fn configured_instance_ids(&self) -> Result<Vec<String>> {
        let owner = self.owner.clone();
        tokio::task::spawn_blocking(move || {
            let transaction = owner.database.begin_read()?;
            let table = transaction.open_table(CURRENT)?;
            table
                .iter()?
                .map(|entry| {
                    let (id, _) = entry?;
                    Ok(id.value().to_owned())
                })
                .collect()
        })
        .await
        .context("configuration identity discovery worker failed")?
    }

    /// Seed a previously unconfigured instance without replacing accepted state,
    /// including an explicitly accepted empty definition at revision zero.
    /// The exclusive session spans the existence check and the ordinary atomic
    /// definition/receipt commit. No component construction occurs here.
    pub async fn initialize_if_absent(
        &self,
        instance: &str,
        request_id: &str,
        desired: &DesiredInstance,
    ) -> Result<bool> {
        self.initialize_if_absent_with(instance, request_id, || Ok(desired.clone()))
            .await
    }

    /// Invoke the host's seed validation only when no accepted definition exists.
    /// Obsolete startup seeds must not override or prevent loading stored state.
    pub async fn initialize_if_absent_with(
        &self,
        instance: &str,
        request_id: &str,
        seed: impl FnOnce() -> Result<DesiredInstance> + Send,
    ) -> Result<bool> {
        let session = self.open(instance).await?;
        let owner = self.owner.clone();
        let id = instance.to_owned();
        let retained_session = session.clone();
        let initialized = tokio::task::spawn_blocking(move || -> Result<bool> {
            let _session = retained_session;
            let transaction = owner.database.begin_read()?;
            let table = transaction.open_table(CURRENT)?;
            Ok(table.get(id.as_str())?.is_some())
        })
        .await
        .context("configuration initialization worker failed")??;
        let result = if initialized {
            Ok(false)
        } else {
            let desired = seed()?.normalized()?;
            session
                .commit(0, request_id, &desired, None, &[])
                .await
                .map(|_| true)
        };
        session.close().await?;
        result
    }

    pub fn new(path: impl AsRef<Path>, key: [u8; 32]) -> Result<Self> {
        Self::new_with_options(path, key, ConfigurationStoreOptions::default())
    }

    pub fn new_with_options(
        path: impl AsRef<Path>,
        key: [u8; 32],
        options: ConfigurationStoreOptions,
    ) -> Result<Self> {
        let database =
            Database::create(path.as_ref()).context("open graph configuration database")?;
        let owner = Arc::new(DatabaseOwner {
            database,
            cipher: RwLock::new(XChaCha20Poly1305::new((&key).into())),
            leases: Mutex::new(BTreeMap::new()),
            options,
            rotating: AtomicBool::new(false),
            key_unconfirmed: AtomicBool::new(false),
        });
        let mut transaction = owner.database.begin_write()?;
        transaction.set_durability(Durability::Immediate);
        transaction.open_table(CURRENT)?;
        transaction.open_table(REQUESTS)?;
        transaction.open_table(SNAPSHOTS)?;
        let version = {
            let mut metadata = transaction.open_table(METADATA)?;
            let existing = metadata
                .get("key-check")?
                .map(|value| value.value().to_vec());
            if let Some(bytes) = existing {
                let version: u32 = unseal(&owner, "drasi.configuration.key-check", &bytes)?;
                anyhow::ensure!(
                    matches!(
                        version,
                        UUID_CONFIGURATION_FORMAT | UUID_EXPIRING_CONFIGURATION_FORMAT
                    ),
                    "unsupported configuration database format {version}; use a fresh database for UUID identities and accepted update recovery"
                );
                version
            } else {
                let bytes = seal(
                    &owner,
                    "drasi.configuration.key-check",
                    &UUID_CONFIGURATION_FORMAT,
                )?;
                metadata.insert("key-check", bytes.as_slice())?;
                UUID_CONFIGURATION_FORMAT
            }
        };
        housekeeping::initialize_policy(&owner, &transaction, version)?;
        transaction.commit()?;
        #[cfg(unix)]
        {
            let parent = path
                .as_ref()
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(parent)?
                .sync_all()
                .context("sync configuration database directory")?;
        }
        Ok(Self { owner })
    }
}

struct Lease {
    owner: Arc<DatabaseOwner>,
    instance: String,
    token: Uuid,
    closed: Arc<tokio::sync::Mutex<bool>>,
}

impl Lease {
    fn release(&self) {
        let mut leases = self.owner.leases.lock().unwrap_or_else(|error| {
            log::error!("Configuration ownership lock poisoned during release");
            error.into_inner()
        });
        if leases.get(&self.instance) == Some(&self.token) {
            leases.remove(&self.instance);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

struct Session(Arc<Lease>);

impl Session {
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Lease) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let lease = self.0.clone();
        let guard = lease.closed.clone().lock_owned().await;
        if *guard {
            return Err(ManagementError::Closed.into());
        }
        // The owned guard and lease stay with blocking I/O if the caller cancels.
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            operation(&lease)
        })
        .await
        .context("configuration storage worker failed")?
    }
}

fn key(instance: &str, name: &str) -> Result<String> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 1024,
        "invalid configuration record name"
    );
    Ok(serde_json::to_string(&(instance, name))?)
}

fn seal<T: Serialize>(owner: &DatabaseOwner, context: &str, value: &T) -> Result<Vec<u8>> {
    let plaintext = serde_json::to_vec(value)?;
    let cipher = owner
        .cipher
        .read()
        .map_err(|_| anyhow::anyhow!("configuration encryption lock poisoned"))?;
    seal_bytes(&cipher, context, &plaintext)
}

fn seal_bytes(cipher: &XChaCha20Poly1305, context: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        plaintext.len() <= MAX_RECORD_BYTES,
        "configuration record exceeds 64 MiB"
    );
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: context.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("configuration encryption failed"))?;
    let mut bytes = Vec::with_capacity(1 + nonce.len() + ciphertext.len());
    bytes.push(1);
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&ciphertext);
    Ok(bytes)
}

fn unseal<T: DeserializeOwned>(owner: &DatabaseOwner, context: &str, bytes: &[u8]) -> Result<T> {
    let cipher = owner
        .cipher
        .read()
        .map_err(|_| anyhow::anyhow!("configuration encryption lock poisoned"))?;
    let plaintext = unseal_bytes(&cipher, context, bytes)?;
    serde_json::from_slice(&plaintext).context("invalid stored configuration")
}

fn unseal_bytes(cipher: &XChaCha20Poly1305, context: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        bytes.len() >= 41 && bytes.len() <= MAX_RECORD_BYTES + 41 && bytes[0] == 1,
        "invalid encrypted configuration record"
    );
    cipher
        .decrypt(
            &XNonce::from(<[u8; 24]>::try_from(&bytes[1..25])?),
            Payload {
                msg: &bytes[25..],
                aad: context.as_bytes(),
            },
        )
        .map_err(|_| {
            anyhow::anyhow!("configuration authentication failed: wrong key or damaged record")
        })
}

fn current(
    owner: &DatabaseOwner,
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    id: &str,
) -> Result<CommittedConfiguration> {
    match table.get(id)? {
        Some(value) => unseal(owner, &format!("current:{id}"), value.value()),
        None => Ok(CommittedConfiguration::default()),
    }
}

#[derive(Serialize, Deserialize)]
struct RequestRecord {
    receipt: AcceptanceReceipt,
    desired: DesiredInstance,
}

#[async_trait]
impl ConfigurationStore for RedbConfigurationStore {
    fn guarantees(&self) -> ConfigurationStoreGuarantees {
        ConfigurationStoreGuarantees::DURABLE
    }

    async fn open(&self, instance_id: &str) -> Result<Arc<dyn ConfigurationSession>> {
        drasi_lib::computation::v1::validate_identifier("instance", instance_id)?;
        let session = {
            let mut leases = self
                .owner
                .leases
                .lock()
                .map_err(|_| anyhow::anyhow!("configuration ownership lock poisoned"))?;
            anyhow::ensure!(
                !self.owner.rotating.load(Ordering::Acquire),
                "configuration encryption key rotation is in progress"
            );
            anyhow::ensure!(
                !self.owner.key_unconfirmed.load(Ordering::Acquire),
                "configuration encryption key is unconfirmed; reopen the database"
            );
            if leases.contains_key(instance_id) {
                return Err(ManagementError::AlreadyOwned(instance_id.to_owned()).into());
            }
            let token = Uuid::new_v4();
            leases.insert(instance_id.to_owned(), token);
            Arc::new(Session(Arc::new(Lease {
                owner: self.owner.clone(),
                instance: instance_id.to_owned(),
                token,
                closed: Arc::new(tokio::sync::Mutex::new(false)),
            })))
        };
        if self.owner.options.receipt_batch_capacity.is_some() {
            session.run(housekeeping::validate_window).await?;
        }
        Ok(session)
    }
}

#[async_trait]
impl ConfigurationSession for Session {
    async fn new_request_id(&self) -> Result<String> {
        self.run(housekeeping::new_request_id).await
    }

    async fn load(&self) -> Result<CommittedConfiguration> {
        self.run(|lease| {
            let transaction = lease.owner.database.begin_read()?;
            current(
                &lease.owner,
                &transaction.open_table(CURRENT)?,
                &lease.instance,
            )
        })
        .await
    }

    async fn commit(
        &self,
        expected_revision: u64,
        request_id: &str,
        desired: &DesiredInstance,
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> Result<AcceptanceReceipt> {
        let request_id = request_id.to_owned();
        let desired = desired.clone();
        let creation = creation.cloned();
        let updates = updates.to_vec();
        self.run(move |lease| {
            let record_key = key(&lease.instance, &request_id)?;
            let mut transaction = lease.owner.database.begin_write()?;
            transaction.set_durability(Durability::Immediate);
            let receipt;
            {
                let mut window = housekeeping::check_request(
                    lease,
                    &transaction.open_table(METADATA)?,
                    &request_id,
                )?;
                let mut requests = transaction.open_table(REQUESTS)?;
                if let Some(value) = requests.get(record_key.as_str())? {
                    let prior: RequestRecord = unseal(
                        &lease.owner,
                        &format!("request:{record_key}"),
                        value.value(),
                    )?;
                    if prior.desired != desired
                        || prior.receipt.creation != creation
                        || prior.receipt.updates != updates
                    {
                        return Err(ManagementError::RequestConflict.into());
                    }
                    return Ok(prior.receipt);
                }
                if let Some(window) = &mut window {
                    window.accept(lease.owner.options)?;
                }
                let mut configurations = transaction.open_table(CURRENT)?;
                let old = current(&lease.owner, &configurations, &lease.instance)?;
                if old.revision != expected_revision {
                    return Err(ManagementError::RevisionConflict {
                        expected: expected_revision,
                        actual: old.revision,
                    }
                    .into());
                }
                let revision = if desired == old.desired {
                    old.revision
                } else {
                    old.revision
                        .checked_add(1)
                        .context("configuration revision exhausted")?
                };
                let mut pending = old.pending_updates;
                for update in &updates {
                    anyhow::ensure!(
                        !pending
                            .iter()
                            .any(|previous| previous.component_id == update.component_id),
                        "component already has an incomplete accepted update"
                    );
                    pending.push(update.clone());
                }
                pending.sort_by(|left, right| left.component_id.cmp(&right.component_id));
                receipt = AcceptanceReceipt {
                    request_id,
                    revision,
                    durable: true,
                    creation,
                    updates,
                };
                let bytes = seal(
                    &lease.owner,
                    &format!("current:{}", lease.instance),
                    &CommittedConfiguration {
                        revision,
                        desired: desired.clone(),
                        pending_updates: pending,
                    },
                )?;
                configurations.insert(lease.instance.as_str(), bytes.as_slice())?;
                let bytes = seal(
                    &lease.owner,
                    &format!("request:{record_key}"),
                    &RequestRecord {
                        receipt: receipt.clone(),
                        desired,
                    },
                )?;
                requests.insert(record_key.as_str(), bytes.as_slice())?;
                if let Some(window) = window {
                    window.save(lease, &transaction)?;
                }
            }
            transaction
                .commit()
                .context("commit graph configuration; resolve request ID before retrying")?;
            Ok(receipt)
        })
        .await
    }

    async fn complete_updates(
        &self,
        expected_revision: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> Result<CommittedConfiguration> {
        let updates = updates.to_vec();
        self.run(move |lease| {
            let mut transaction = lease.owner.database.begin_write()?;
            transaction.set_durability(Durability::Immediate);
            let configuration;
            {
                let mut table = transaction.open_table(CURRENT)?;
                let mut record = current(&lease.owner, &table, &lease.instance)?;
                anyhow::ensure!(
                    record.revision == expected_revision,
                    ManagementError::RevisionConflict {
                        expected: expected_revision,
                        actual: record.revision
                    }
                );
                for update in &updates {
                    if let Some(pending) = record
                        .pending_updates
                        .iter()
                        .find(|pending| pending.component_id == update.component_id)
                    {
                        anyhow::ensure!(
                            pending.operation_id == update.operation_id,
                            ManagementError::RequestConflict
                        );
                    }
                }
                let before = record.pending_updates.len();
                record.pending_updates.retain(|pending| {
                    !updates.iter().any(|update| {
                        update.component_id == pending.component_id
                            && update.operation_id == pending.operation_id
                    })
                });
                if record.pending_updates.len() != before {
                    let bytes = seal(
                        &lease.owner,
                        &format!("current:{}", lease.instance),
                        &record,
                    )?;
                    table.insert(lease.instance.as_str(), bytes.as_slice())?;
                }
                configuration = record;
            }
            transaction.commit().context(
                "confirm component update completion; reload before resuming graph processing",
            )?;
            Ok(configuration)
        })
        .await
    }

    async fn receipt(&self, request_id: &str) -> Result<Option<AcceptanceReceipt>> {
        let request_id = request_id.to_owned();
        self.run(move |lease| {
            let record_key = key(&lease.instance, &request_id)?;
            let transaction = lease.owner.database.begin_read()?;
            housekeeping::check_request(lease, &transaction.open_table(METADATA)?, &request_id)?;
            let table = transaction.open_table(REQUESTS)?;
            let value = table.get(record_key.as_str())?;
            value
                .map(|value| {
                    unseal::<RequestRecord>(
                        &lease.owner,
                        &format!("request:{record_key}"),
                        value.value(),
                    )
                    .map(|request| request.receipt)
                })
                .transpose()
        })
        .await
    }

    async fn snapshot(&self, name: &str) -> Result<CommittedConfiguration> {
        let name = name.to_owned();
        self.run(move |lease| {
            let record_key = key(&lease.instance, &name)?;
            let mut transaction = lease.owner.database.begin_write()?;
            transaction.set_durability(Durability::Immediate);
            let configuration;
            {
                let mut snapshots = transaction.open_table(SNAPSHOTS)?;
                if snapshots.get(record_key.as_str())?.is_some() {
                    return Err(ManagementError::SnapshotExists(name).into());
                }
                configuration = current(
                    &lease.owner,
                    &transaction.open_table(CURRENT)?,
                    &lease.instance,
                )?;
                let bytes = seal(
                    &lease.owner,
                    &format!("snapshot:{record_key}"),
                    &configuration,
                )?;
                snapshots.insert(record_key.as_str(), bytes.as_slice())?;
            }
            transaction.commit()?;
            Ok(configuration)
        })
        .await
    }

    async fn load_snapshot(&self, name: &str) -> Result<Option<CommittedConfiguration>> {
        let name = name.to_owned();
        self.run(move |lease| {
            let record_key = key(&lease.instance, &name)?;
            let transaction = lease.owner.database.begin_read()?;
            let table = transaction.open_table(SNAPSHOTS)?;
            let value = table.get(record_key.as_str())?;
            value
                .map(|value| {
                    unseal(
                        &lease.owner,
                        &format!("snapshot:{record_key}"),
                        value.value(),
                    )
                })
                .transpose()
        })
        .await
    }

    async fn list_snapshots(
        &self,
        after: Option<&str>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ConfigurationSnapshotSummary>> {
        let after = after.map(str::to_owned);
        self.run(move |lease| housekeeping::list_snapshots(lease, after.as_deref(), limit))
            .await
    }

    async fn delete_snapshot(&self, name: &str, expected_revision: u64) -> Result<bool> {
        let name = name.to_owned();
        self.run(move |lease| housekeeping::delete_snapshot(lease, &name, expected_revision))
            .await
    }

    async fn close(&self) -> Result<()> {
        let mut closed = self.0.closed.lock().await;
        *closed = true;
        self.0.release();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::computation::v1::{
        ComponentAddition, ComponentDescriptor, ComponentId, ComponentName, ComponentRole,
        ComponentSpecification, ImplementationIdentity,
    };
    use drasi_lib::management::{AcceptedComponentIdentity, ComponentIdAllocation};

    fn creation(id: ComponentId, digest: [u8; 32]) -> AcceptedComponentCreation {
        AcceptedComponentCreation {
            components: vec![AcceptedComponentIdentity {
                id: id.clone(),
                name: ComponentName::try_new("orders").unwrap(),
            }],
            allocations: vec![ComponentIdAllocation {
                requested: None,
                assigned: id,
            }],
            request_digest: digest,
        }
    }

    fn component_definition(id: ComponentId) -> Result<DesiredInstance> {
        let specification = ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, Vec::new())?,
            role: ComponentRole::Service,
            completion: None,
            implementation: ImplementationIdentity::try_new("identity/service", "1")?,
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        };
        let mut desired = DesiredInstance::default();
        desired.topology.components.push(
            ComponentAddition::declaration(ComponentName::try_new("orders")?, specification)
                .auto_start(false)
                .definition,
        );
        desired.normalized()
    }

    #[tokio::test]
    async fn creation_receipt_preserves_assigned_uuid_after_deletion_and_reopen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("creation.redb");
        let creation = creation(ComponentId::generate(), [1; 32]);
        let desired = component_definition(creation.components[0].id.clone())?;
        let replacement = component_definition(ComponentId::generate())?;
        let receipt = {
            let store = RedbConfigurationStore::new(&path, [18; 32])?;
            let session = store.open("instance").await?;
            let receipt = session
                .commit(0, "create", &desired, Some(&creation), &[])
                .await?;
            assert_eq!(receipt.creation.as_ref(), Some(&creation));
            assert_eq!(session.load().await?.desired, desired);
            session.snapshot("original").await?;
            let deletion = session
                .commit(
                    receipt.revision,
                    "delete",
                    &DesiredInstance::default(),
                    None,
                    &[],
                )
                .await?;
            session
                .commit(deletion.revision, "replacement", &replacement, None, &[])
                .await?;
            session.close().await?;
            receipt
        };
        let store = RedbConfigurationStore::new(&path, [18; 32])?;
        let session = store.open("instance").await?;
        assert_eq!(session.receipt("create").await?, Some(receipt.clone()));
        assert_eq!(
            session
                .commit(0, "create", &desired, Some(&creation), &[])
                .await?,
            receipt
        );
        assert_eq!(session.load().await?.desired, replacement);
        assert_eq!(
            session.load_snapshot("original").await?.unwrap().desired,
            desired
        );
        for changed in [
            None,
            Some(AcceptedComponentCreation {
                components: vec![AcceptedComponentIdentity {
                    id: ComponentId::generate(),
                    name: ComponentName::try_new("orders")?,
                }],
                ..creation.clone()
            }),
            Some(AcceptedComponentCreation {
                allocations: vec![ComponentIdAllocation {
                    requested: None,
                    assigned: ComponentId::generate(),
                }],
                ..creation.clone()
            }),
            Some(AcceptedComponentCreation {
                request_digest: [2; 32],
                ..creation.clone()
            }),
        ] {
            let error = session
                .commit(0, "create", &desired, changed.as_ref(), &[])
                .await
                .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<ManagementError>(),
                Some(ManagementError::RequestConflict)
            ));
        }
        assert_eq!(session.load().await?.desired, replacement);
        session.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn batch_receipt_preserves_all_assignments_after_removal_and_reopen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("batch-creation.redb");
        let owner = ComponentId::generate();
        let second = ComponentId::generate();
        let helper = ComponentId::derive_owned(&owner, "timer")?;
        let mut desired = DesiredInstance::default();
        let mut components = Vec::new();
        for (id, name) in [
            (owner.clone(), "orders"),
            (second.clone(), "alerts"),
            (helper, "timer"),
        ] {
            let mut member = component_definition(id.clone())?;
            member.topology.components[0].name = ComponentName::try_new(name)?;
            desired
                .topology
                .components
                .extend(member.topology.components);
            components.push(AcceptedComponentIdentity {
                id,
                name: ComponentName::try_new(name)?,
            });
        }
        let desired = desired.normalized()?;
        let creation = AcceptedComponentCreation {
            components,
            allocations: vec![
                ComponentIdAllocation {
                    requested: None,
                    assigned: owner,
                },
                ComponentIdAllocation {
                    requested: None,
                    assigned: second,
                },
            ],
            request_digest: [3; 32],
        };
        let receipt = {
            let store = RedbConfigurationStore::new(&path, [20; 32])?;
            assert_eq!(store.guarantees(), ConfigurationStoreGuarantees::DURABLE);
            let session = store.open("instance").await?;
            let receipt = session
                .commit(0, "batch", &desired, Some(&creation), &[])
                .await?;
            session
                .commit(
                    receipt.revision,
                    "remove",
                    &DesiredInstance::default(),
                    None,
                    &[],
                )
                .await?;
            session.close().await?;
            receipt
        };
        let store = RedbConfigurationStore::new(&path, [20; 32])?;
        let session = store.open("instance").await?;
        assert_eq!(session.receipt("batch").await?, Some(receipt.clone()));
        assert_eq!(
            session
                .commit(0, "batch", &desired, Some(&creation), &[])
                .await?,
            receipt
        );
        assert!(session.load().await?.desired.topology.components.is_empty());
        assert_eq!(receipt.creation.as_ref().unwrap().components.len(), 3);
        assert_eq!(receipt.creation.as_ref().unwrap().allocations.len(), 2);
        session.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn previous_configuration_formats_are_rejected_without_rewriting_records() -> Result<()> {
        for version in [1u32, 2, 3, 4, 5, 6] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("old-format.redb");
            let (marker, current_bytes) = {
                let store = RedbConfigurationStore::new(&path, [19; 32])?;
                let session = store.open("instance").await?;
                session
                    .commit(0, "original", &DesiredInstance::default(), None, &[])
                    .await?;
                session.close().await?;
                let marker = seal(&store.owner, "drasi.configuration.key-check", &version)?;
                let transaction = store.owner.database.begin_write()?;
                transaction
                    .open_table(METADATA)?
                    .insert("key-check", marker.as_slice())?;
                transaction.commit()?;
                let transaction = store.owner.database.begin_read()?;
                let current_bytes = transaction
                    .open_table(CURRENT)?
                    .get("instance")?
                    .unwrap()
                    .value()
                    .to_vec();
                (marker, current_bytes)
            };
            let error = RedbConfigurationStore::new(&path, [19; 32])
                .err()
                .expect("old identity format must reject");
            assert!(error.to_string().contains("use a fresh database"));
            let database = Database::create(&path)?;
            let transaction = database.begin_read()?;
            assert_eq!(
                transaction
                    .open_table(METADATA)?
                    .get("key-check")?
                    .unwrap()
                    .value(),
                marker
            );
            assert_eq!(
                transaction
                    .open_table(CURRENT)?
                    .get("instance")?
                    .unwrap()
                    .value(),
                current_bytes
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialization_preserves_accepted_empty_revision_zero_across_reopen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("configuration.redb");
        let mut changed = DesiredInstance::default();
        changed.topology.requirements = drasi_lib::computation::v1::PipeRequirements::new([
            drasi_lib::computation::v1::PipeCapability::Backpressure,
        ]);
        {
            let store = RedbConfigurationStore::new(&path, [17; 32])?;
            assert!(store.configured_instance_ids().await?.is_empty());
            assert!(
                store
                    .initialize_if_absent("first", "initial", &DesiredInstance::default())
                    .await?
            );
            assert_eq!(store.configured_instance_ids().await?, ["first"]);
            assert!(
                !store
                    .initialize_if_absent("first", "changed", &changed)
                    .await?
            );
            let session = store.open("first").await?;
            assert_eq!(session.load().await?, CommittedConfiguration::default());
            assert!(session.receipt("initial").await?.unwrap().durable);
            assert!(session.receipt("changed").await?.is_none());
            assert!(store
                .initialize_if_absent("first", "racing", &changed)
                .await
                .unwrap_err()
                .downcast_ref::<ManagementError>()
                .is_some());
            session.close().await?;
            let external = store.open("external").await?;
            external
                .commit(
                    0,
                    "accepted-elsewhere",
                    &DesiredInstance::default(),
                    None,
                    &[],
                )
                .await?;
            external.close().await?;
        }
        let store = RedbConfigurationStore::new(&path, [17; 32])?;
        assert_eq!(
            store.configured_instance_ids().await?,
            ["external", "first"]
        );
        for instance in ["first", "external"] {
            assert!(
                !store
                    .initialize_if_absent_with(instance, "obsolete", || {
                        anyhow::bail!("obsolete seed must not be evaluated")
                    })
                    .await?
            );
            assert!(
                !store
                    .initialize_if_absent(instance, "changed", &changed)
                    .await?
            );
            let session = store.open(instance).await?;
            assert_eq!(session.load().await?, CommittedConfiguration::default());
            session.close().await?;
        }
        assert!(
            store
                .initialize_if_absent("second", "initial", &changed)
                .await?
        );
        let session = store.open("second").await?;
        assert_eq!(session.load().await?.desired, changed);
        session.close().await?;
        let mut invalid = changed.clone();
        invalid.version = 3;
        assert!(store
            .initialize_if_absent("third", "invalid", &invalid)
            .await
            .is_err());
        assert!(
            store
                .initialize_if_absent("third", "initial", &DesiredInstance::default())
                .await?
        );
        let session = store.open("third").await?;
        assert_eq!(session.load().await?, CommittedConfiguration::default());
        assert!(session.receipt("invalid").await?.is_none());
        session.close().await?;
        Ok(())
    }
}
