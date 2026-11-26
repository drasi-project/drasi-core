// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One sender mapping, not another general-purpose delivery runner. The upstream
//! acknowledged pipe retains ownership until the remote receipt is saved. A
//! durable sender keeps the mapping in a synced local file it owns.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use drasi_computation_contracts::{
    decode_bounded_messagepack, encode_bounded_messagepack, DeliveryBatchIdentity, DeliveryScope,
    InputEnvelope, ProducerSession,
};
use drasi_core_types::interface::StorageDurability;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    codec::{preflight_messagepack, PortableEnvelopeCodec},
    config::LinkContract,
    protocol::{AcceptanceBoundary, Receipt},
    BridgeError, ErrorCode, Profile, Result,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalInput {
    pub stream: String,
    pub sequence: u64,
    pub digest: [u8; 32],
}

impl LogicalInput {
    pub fn identify(
        profile: Profile,
        scope: &DeliveryScope,
        input: &InputEnvelope,
        codec: &PortableEnvelopeCodec,
        bytes: &[u8],
    ) -> Result<Self> {
        match profile {
            Profile::Durable => {
                let identity =
                    DeliveryBatchIdentity::for_input(scope, input, &codec.logical_codec()?)
                        .map_err(|error| BridgeError::invalid(error.to_string()))?;
                Ok(Self {
                    stream: identity.stream().into(),
                    sequence: identity.sequence(),
                    digest: *identity.content_digest(),
                })
            }
            Profile::Volatile => Ok(Self {
                stream: input.envelope.system().stream().as_str().into(),
                sequence: input.envelope.system().sequence(),
                digest: Sha256::digest(bytes).into(),
            }),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub input: LogicalInput,
    pub client_sequence: u64,
    /// Preserve the exact first-admission representation when upstream transport
    /// IDs/timings change on replay. Only this one pending frame is retained.
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
    pub wire_digest: [u8; 32],
    pub receipt: Option<Receipt>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    contract: [u8; 32],
    session: Option<ProducerSession>,
    last: Option<Mapping>,
}

/// The durable sender mapping file, `<directory>/<namespace>.mapping`, with an
/// exclusive `<namespace>.lock` held while a sender owns it. The namespace is
/// derived from the sender instance, graph and endpoint, so distinct senders
/// sharing a directory never share a mapping.
pub struct MappingFile {
    path: PathBuf,
    lock: PathBuf,
    owner: Option<std::fs::File>,
}

impl MappingFile {
    pub fn new(
        directory: &Path,
        sender_instance: &str,
        graph_id: &str,
        sender_endpoint: &str,
    ) -> anyhow::Result<Self> {
        let namespace = serde_json::to_vec(&(sender_instance, graph_id, sender_endpoint))?;
        let name = format!("{:x}", Sha256::digest(namespace));
        Ok(Self {
            path: directory.join(format!("{name}.mapping")),
            lock: directory.join(format!("{name}.lock")),
            owner: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Takes exclusive ownership of the mapping for this process lifetime.
    fn acquire(&mut self) -> anyhow::Result<()> {
        if self.owner.is_some() {
            return Ok(());
        }
        let directory = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("sender mapping path has no directory"))?;
        std::fs::create_dir_all(directory)?;
        let lock = private_options().read(true).write(true).open(&self.lock)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(BridgeError::new(
                    ErrorCode::Busy,
                    "another sender owns this bridge mapping",
                )
                .into())
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        self.owner = Some(lock);
        Ok(())
    }

    fn release(&mut self) {
        self.owner = None;
    }

    fn read(&self, limit: usize) -> anyhow::Result<Option<Vec<u8>>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= limit,
            "sender mapping file is empty or exceeds its bound"
        );
        Ok(Some(bytes))
    }

    /// Replaces the mapping atomically: a crash leaves either the old or the
    /// new record, synced to the device before the call returns. File I/O
    /// blocks, so it runs off the async workers; an error or cancellation
    /// leaves the previous record in place.
    async fn write(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        anyhow::ensure!(self.owner.is_some(), "sender mapping is not owned");
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let directory = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("sender mapping path has no directory"))?;
            let temporary = path.with_extension("mapping.tmp");
            {
                let mut file = private_options()
                    .write(true)
                    .truncate(true)
                    .open(&temporary)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            std::fs::rename(&temporary, &path)?;
            sync_directory(directory)
        })
        .await?
    }

    /// Removes the mapping idempotently: an absent file is already erased.
    pub fn erase(&self) -> anyhow::Result<()> {
        for path in [&self.path, &self.path.with_extension("mapping.tmp")] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(directory) = self.path.parent().filter(|path| path.is_dir()) {
            sync_directory(directory)?;
        }
        Ok(())
    }
}

fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn sync_directory(directory: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

pub struct SenderState {
    contract: LinkContract,
    store: Option<MappingFile>,
    ledger: Ledger,
    loaded: bool,
}

impl SenderState {
    pub fn new(contract: LinkContract, store: Option<MappingFile>) -> anyhow::Result<Self> {
        match &store {
            Some(_) => {
                anyhow::ensure!(
                    contract.profile == Profile::Durable,
                    "volatile mapping cannot own durable storage"
                );
                // A synced local file survives process restarts and power loss,
                // never the loss of the machine's storage.
                StorageDurability::LOCAL_POWER_LOSS.require(contract.failure_scope)?;
            }
            None => anyhow::ensure!(
                contract.profile == Profile::Volatile,
                "durable mapping requires actual storage"
            ),
        }
        Ok(Self {
            ledger: Ledger {
                version: 1,
                contract: contract.fingerprint(),
                session: None,
                last: None,
            },
            contract,
            store,
            loaded: false,
        })
    }

    fn byte_limit(&self) -> usize {
        self.contract.limits.max_envelope_bytes + 16 * 1024
    }

    pub async fn load(&mut self) -> anyhow::Result<()> {
        if self.loaded {
            return Ok(());
        }
        let limit = self.byte_limit();
        if let Some(store) = &mut self.store {
            store.acquire()?;
            if let Some(bytes) = store.read(limit)? {
                preflight_messagepack(&bytes, limit, 4096)?;
                self.ledger = decode_bounded_messagepack(&bytes, limit)?;
            }
        }
        self.validate(&self.ledger)?;
        self.loaded = true;
        Ok(())
    }

    fn validate(&self, ledger: &Ledger) -> Result<()> {
        if ledger.version != 1 || ledger.contract != self.contract.fingerprint() {
            return Err(BridgeError::new(
                ErrorCode::Incompatible,
                "saved sender contract changed",
            ));
        }
        if let Some(mapping) = &ledger.last {
            let session = ledger
                .session
                .as_ref()
                .ok_or_else(|| BridgeError::invalid("mapping has no receiver session"))?;
            if mapping.client_sequence == 0
                || mapping.input.sequence == 0
                || mapping.envelope.len() > self.contract.limits.max_envelope_bytes
            {
                return Err(BridgeError::invalid(
                    "invalid saved sender sequence or frame limit",
                ));
            }
            crate::config::identifier(&mapping.input.stream)?;
            match &mapping.receipt {
                None => {
                    let digest: [u8; 32] = Sha256::digest(&mapping.envelope).into();
                    if mapping.envelope.is_empty() || digest != mapping.wire_digest {
                        return Err(BridgeError::invalid(
                            "saved sender pending frame is inconsistent",
                        ));
                    }
                }
                Some(receipt) => {
                    if !mapping.envelope.is_empty()
                        || receipt.admission.session != *session
                        || receipt.admission.sequence != mapping.client_sequence
                        || receipt.admission.position == 0
                        || receipt.boundary != AcceptanceBoundary::for_contract(&self.contract)
                        || receipt.contract != ledger.contract
                    {
                        return Err(BridgeError::invalid("saved sender receipt is inconsistent"));
                    }
                }
            }
        }
        Ok(())
    }

    async fn save(&mut self, ledger: Ledger) -> anyhow::Result<()> {
        self.validate(&ledger)?;
        let bytes = encode_bounded_messagepack(&ledger, self.byte_limit())?;
        if let Some(store) = &self.store {
            store.write(bytes).await?;
        }
        // Publication follows the actual commit; the old mapping is never
        // re-used as fresh input.
        self.ledger = ledger;
        Ok(())
    }

    pub fn session(&self) -> Option<&ProducerSession> {
        self.ledger.session.as_ref()
    }

    pub fn last(&self) -> Option<&Mapping> {
        self.ledger.last.as_ref()
    }

    pub async fn bind_session(&mut self, session: ProducerSession) -> anyhow::Result<()> {
        self.load().await?;
        if let Some(existing) = self.session() {
            anyhow::ensure!(
                existing == &session,
                BridgeError::new(
                    ErrorCode::StateLost,
                    "receiver session changed; do not assign a fresh retry identity"
                )
            );
            return Ok(());
        }
        let mut ledger = self.ledger.clone();
        ledger.session = Some(session);
        self.save(ledger).await
    }

    pub async fn prepare(&mut self, input: LogicalInput, envelope: Vec<u8>) -> anyhow::Result<()> {
        self.load().await?;
        anyhow::ensure!(
            self.session().is_some(),
            "receiver session must be saved before sender admission"
        );
        let next = if let Some(last) = self.last() {
            if last.input.stream != input.stream {
                return Err(BridgeError::new(
                    ErrorCode::Conflict,
                    "logical producer changed under the same link",
                )
                .into());
            }
            if input.sequence < last.input.sequence {
                return Err(BridgeError::new(
                    ErrorCode::Expired,
                    "upstream logical input predates retained sender receipt",
                )
                .into());
            }
            if input.sequence == last.input.sequence {
                anyhow::ensure!(
                    last.input == input,
                    BridgeError::new(
                        ErrorCode::Conflict,
                        "logical input was retried with different content"
                    )
                );
                return Ok(());
            }
            anyhow::ensure!(
                last.receipt.is_some(),
                BridgeError::new(
                    ErrorCode::AcceptanceUnknown,
                    "resolve the outstanding input before admitting another"
                )
            );
            last.client_sequence
                .checked_add(1)
                .ok_or_else(|| BridgeError::invalid("client sequences exhausted"))?
        } else {
            1
        };
        let mut ledger = self.ledger.clone();
        ledger.last = Some(Mapping {
            input,
            client_sequence: next,
            wire_digest: Sha256::digest(&envelope).into(),
            envelope,
            receipt: None,
        });
        self.save(ledger).await
    }

    pub async fn complete(&mut self, receipt: Receipt) -> anyhow::Result<()> {
        let mut ledger = self.ledger.clone();
        let mapping = ledger
            .last
            .as_mut()
            .ok_or_else(|| BridgeError::invalid("receipt without pending input"))?;
        anyhow::ensure!(
            Some(&receipt.admission.session) == ledger.session.as_ref()
                && receipt.admission.sequence == mapping.client_sequence
                && receipt.admission.position > 0
                && receipt.contract == ledger.contract
                && receipt.boundary == AcceptanceBoundary::for_contract(&self.contract),
            BridgeError::new(
                ErrorCode::Conflict,
                "receipt does not prove the selected admission boundary"
            )
        );
        mapping.receipt = Some(receipt);
        mapping.envelope.clear();
        self.save(ledger).await
    }

    /// Releases ownership of the mapping; the saved record stays.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        if let Some(store) = &mut self.store {
            store.release();
        }
        Ok(())
    }
}
