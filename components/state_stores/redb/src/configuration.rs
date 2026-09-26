// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    XChaCha20Poly1305, XNonce,
};
use drasi_lib::management::{
    AcceptanceReceipt, CommittedConfiguration, ConfigurationSession, ConfigurationStore,
    DesiredInstance, ManagementError,
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

struct DatabaseOwner {
    database: Database,
    cipher: XChaCha20Poly1305,
    leases: Mutex<BTreeMap<String, Uuid>>,
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
    pub fn new(path: impl AsRef<Path>, key: [u8; 32]) -> Result<Self> {
        let database =
            Database::create(path.as_ref()).context("open graph configuration database")?;
        let owner = Arc::new(DatabaseOwner {
            database,
            cipher: XChaCha20Poly1305::new((&key).into()),
            leases: Mutex::new(BTreeMap::new()),
        });
        let mut transaction = owner.database.begin_write()?;
        transaction.set_durability(Durability::Immediate);
        transaction.open_table(CURRENT)?;
        transaction.open_table(REQUESTS)?;
        transaction.open_table(SNAPSHOTS)?;
        {
            let mut metadata = transaction.open_table(METADATA)?;
            let existing = metadata
                .get("key-check")?
                .map(|value| value.value().to_vec());
            if let Some(bytes) = existing {
                let version: u32 = unseal(&owner, "drasi.configuration.key-check", &bytes)?;
                anyhow::ensure!(version == 1, "unsupported configuration database format");
            } else {
                let bytes = seal(&owner, "drasi.configuration.key-check", &1u32)?;
                metadata.insert("key-check", bytes.as_slice())?;
            }
        }
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
    anyhow::ensure!(
        plaintext.len() <= MAX_RECORD_BYTES,
        "configuration record exceeds 64 MiB"
    );
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = owner
        .cipher
        .encrypt(
            &nonce,
            Payload {
                msg: &plaintext,
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
    anyhow::ensure!(
        bytes.len() >= 41 && bytes.len() <= MAX_RECORD_BYTES + 41 && bytes[0] == 1,
        "invalid encrypted configuration record"
    );
    let plaintext = owner
        .cipher
        .decrypt(
            XNonce::from_slice(&bytes[1..25]),
            Payload {
                msg: &bytes[25..],
                aad: context.as_bytes(),
            },
        )
        .map_err(|_| {
            anyhow::anyhow!("configuration authentication failed: wrong key or damaged record")
        })?;
    serde_json::from_slice(&plaintext).context("invalid stored configuration")
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
    async fn open(&self, instance_id: &str) -> Result<Arc<dyn ConfigurationSession>> {
        drasi_lib::computation::v1::ComponentId::try_new(instance_id)?;
        let mut leases = self
            .owner
            .leases
            .lock()
            .map_err(|_| anyhow::anyhow!("configuration ownership lock poisoned"))?;
        if leases.contains_key(instance_id) {
            return Err(ManagementError::AlreadyOwned(instance_id.to_owned()).into());
        }
        let token = Uuid::new_v4();
        leases.insert(instance_id.to_owned(), token);
        Ok(Arc::new(Session(Arc::new(Lease {
            owner: self.owner.clone(),
            instance: instance_id.to_owned(),
            token,
            closed: Arc::new(tokio::sync::Mutex::new(false)),
        }))))
    }
}

#[async_trait]
impl ConfigurationSession for Session {
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
    ) -> Result<AcceptanceReceipt> {
        let request_id = request_id.to_owned();
        let desired = desired.clone();
        self.run(move |lease| {
            let record_key = key(&lease.instance, &request_id)?;
            let mut transaction = lease.owner.database.begin_write()?;
            transaction.set_durability(Durability::Immediate);
            let receipt;
            {
                let mut requests = transaction.open_table(REQUESTS)?;
                if let Some(value) = requests.get(record_key.as_str())? {
                    let prior: RequestRecord = unseal(
                        &lease.owner,
                        &format!("request:{record_key}"),
                        value.value(),
                    )?;
                    if prior.desired != desired {
                        return Err(ManagementError::RequestConflict.into());
                    }
                    return Ok(prior.receipt);
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
                receipt = AcceptanceReceipt {
                    request_id,
                    revision,
                    durable: true,
                };
                let bytes = seal(
                    &lease.owner,
                    &format!("current:{}", lease.instance),
                    &CommittedConfiguration {
                        revision,
                        desired: desired.clone(),
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
            }
            transaction
                .commit()
                .context("commit graph configuration; resolve request ID before retrying")?;
            Ok(receipt)
        })
        .await
    }

    async fn receipt(&self, request_id: &str) -> Result<Option<AcceptanceReceipt>> {
        let request_id = request_id.to_owned();
        self.run(move |lease| {
            let record_key = key(&lease.instance, &request_id)?;
            let transaction = lease.owner.database.begin_read()?;
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

    async fn close(&self) -> Result<()> {
        let mut closed = self.0.closed.lock().await;
        *closed = true;
        self.0.release();
        Ok(())
    }
}
