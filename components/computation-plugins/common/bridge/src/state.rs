// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! One sender mapping, not another general-purpose delivery runner. The upstream
//! acknowledged pipe retains ownership until the remote receipt is saved.

use drasi_core::computation::{ComputationIndexes, ComputationTransaction};
use drasi_lib::computation::v1::{
    decode_bounded_messagepack, encode_bounded_messagepack, DeliveryBatchIdentity, DeliveryScope,
    InputEnvelope, ProducerSession,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    codec::{preflight_messagepack, PortableEnvelopeCodec},
    config::LinkContract,
    protocol::{AcceptanceBoundary, Receipt},
    BridgeError, ErrorCode, Profile, Result,
};

const KEY: &str = "drasi.bridge.sender-mapping.v1";

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

pub struct SenderState {
    contract: LinkContract,
    transaction: Option<ComputationTransaction>,
    ledger: Ledger,
    loaded: bool,
}

impl SenderState {
    pub fn new(
        contract: LinkContract,
        indexes: Option<ComputationIndexes>,
    ) -> anyhow::Result<Self> {
        let transaction = match indexes {
            Some(indexes) => {
                anyhow::ensure!(
                    contract.profile == Profile::Durable,
                    "volatile mapping cannot own durable storage"
                );
                indexes.durability().require(contract.failure_scope)?;
                anyhow::ensure!(
                    indexes.cleanup().is_some(),
                    "sender mapping requires an asynchronous cleanup owner"
                );
                Some(ComputationTransaction::try_new(indexes)?)
            }
            None => {
                anyhow::ensure!(
                    contract.profile == Profile::Volatile,
                    "durable mapping requires actual storage"
                );
                None
            }
        };
        Ok(Self {
            ledger: Ledger {
                version: 1,
                contract: contract.fingerprint(),
                session: None,
                last: None,
            },
            contract,
            transaction,
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
        if let Some(transaction) = &self.transaction {
            let outbox = transaction
                .resources()
                .outbox_writer()
                .expect("validated transaction");
            let (head, records) = transaction
                .run(async {
                    let head = outbox.read_latest_sequence(KEY).await?;
                    let records = if head == Some(1) {
                        outbox.read_from(KEY, 0).await?
                    } else {
                        Vec::new()
                    };
                    Ok((head, records))
                })
                .await?;
            match (head, records.as_slice()) {
                (None, []) => {}
                (Some(1), [(1, bytes)]) => {
                    preflight_messagepack(bytes, self.byte_limit(), 4096)?;
                    self.ledger = decode_bounded_messagepack(bytes, self.byte_limit())?;
                }
                _ => anyhow::bail!(
                    "sender mapping expects one bounded metadata record at sequence one"
                ),
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
        if let Some(transaction) = &self.transaction {
            let outbox = transaction
                .resources()
                .outbox_writer()
                .expect("validated transaction");
            transaction
                .run(async {
                    outbox.append(KEY, 1, &bytes).await?;
                    Ok(())
                })
                .await?;
        }
        // Publication follows the actual commit. Cancellation/uncertainty fences
        // ComputationTransaction; the old mapping is never re-used as fresh input.
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

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        if let Some(transaction) = &self.transaction {
            transaction.shutdown().await?;
        }
        Ok(())
    }
}
