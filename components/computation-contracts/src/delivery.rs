// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Immutable delivery identity and operation data; no progress store or transaction owner.

use crate::{
    output_identity::Candidate, ChangeEnvelope, ChangeOperation, ComponentId, EnvelopeCodec,
    InputEnvelope, PortId,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryScope {
    construction_scope: String,
    graph: String,
    consumer: ComponentId,
}

impl DeliveryScope {
    pub fn construction_scope(&self) -> &str {
        &self.construction_scope
    }
    pub fn graph(&self) -> &str {
        &self.graph
    }
    pub fn consumer(&self) -> &ComponentId {
        &self.consumer
    }
    pub fn new(
        construction_scope: String,
        graph: String,
        consumer: ComponentId,
    ) -> anyhow::Result<Self> {
        let scope = Self {
            construction_scope,
            graph,
            consumer,
        };
        scope.validate()?;
        Ok(scope)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for value in [
            &self.construction_scope,
            &self.graph,
            self.consumer.as_str(),
        ] {
            bounded_identifier(value)?;
        }
        Ok(())
    }
}

/// Opaque, restart-stable operation key. Destination/configuration generations
/// are deliberately excluded: a replacement under the same consumer ID inherits
/// its pending obligations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DeliveryId(String);

impl DeliveryId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeliveryId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub struct DeliveryItem<'a> {
    pub id: DeliveryId,
    pub position: DeliveryPosition,
    pub port: &'a PortId,
    pub envelope: &'a ChangeEnvelope,
    pub operation: &'a ChangeOperation,
    /// Canonical batch digest, excluding replay transport identity and immediate
    /// query completion timings. It is not an idempotency key.
    pub content_digest: [u8; 32],
}

/// A complete batch confirmation, bound to actual content and consumer identity.
/// A remote receiver derives this itself; a caller-supplied digest is not proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryBatchIdentity {
    version: u32,
    stream: String,
    sequence: u64,
    operations: usize,
    digest: [u8; 32],
}

impl DeliveryBatchIdentity {
    /// Derive the same identity used by delivery progress without opening a
    /// progress store or invoking a destination.
    pub fn for_input(
        scope: &DeliveryScope,
        input: &InputEnvelope,
        codec: &EnvelopeCodec,
    ) -> anyhow::Result<Self> {
        scope.validate()?;
        bounded_identifier(input.port.as_str())?;
        bounded_identifier(input.envelope.system().stream().as_str())?;
        batch_identity(scope, input, &Candidate::new(&input.envelope, codec)?)
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn operations(&self) -> usize {
        self.operations
    }

    pub fn content_digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl DeliveryItem<'_> {
    pub fn batch_identity(&self) -> anyhow::Result<DeliveryBatchIdentity> {
        self.position.validate()?;
        anyhow::ensure!(
            self.position.operations == self.envelope.changes().operations().len()
                && std::ptr::eq(
                    self.operation,
                    &self.envelope.changes().operations()[self.position.operation]
                ),
            "delivery position does not match the operation"
        );
        Ok(DeliveryBatchIdentity {
            version: 1,
            stream: self.position.stream.clone(),
            sequence: self.position.sequence,
            operations: self.position.operations,
            digest: self.content_digest,
        })
    }
}

pub fn batch_identity(
    scope: &DeliveryScope,
    input: &InputEnvelope,
    candidate: &Candidate,
) -> anyhow::Result<DeliveryBatchIdentity> {
    let stream = serde_json::to_vec(&(
        "drasi.delivery-stream.v1",
        scope,
        &input.port,
        input.envelope.system().stream(),
        &candidate.producer,
    ))?;
    Ok(DeliveryBatchIdentity {
        version: 1,
        stream: format!("drasi-delivery-stream-v1-{:x}", Sha256::digest(stream)),
        sequence: candidate.sequence,
        operations: input.envelope.changes().operations().len(),
        digest: candidate.digest,
    })
}

/// Ordered destination cursor. Keeping its last batch's digest/completed prefix
/// permits bounded deduplication; older batches must reject, never become new.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryPosition {
    version: u32,
    stream: String,
    sequence: u64,
    operation: usize,
    operations: usize,
}

impl DeliveryPosition {
    pub fn stream(&self) -> &str {
        &self.stream
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn operation(&self) -> usize {
        self.operation
    }
    pub fn operations(&self) -> usize {
        self.operations
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        const PREFIX: &str = "drasi-delivery-stream-v1-";
        anyhow::ensure!(
            self.version == 1
                && self.sequence > 0
                && self.operation < self.operations
                && self.stream.starts_with(PREFIX)
                && self.stream.len() == PREFIX.len() + 64
                && self.stream.as_bytes()[PREFIX.len()..]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
            "invalid ordered delivery position"
        );
        Ok(())
    }
}

/// Success must mean this operation actually completed, never merely queued.
/// A handler may receive the same ID again after cancellation or a failed local
/// progress commit. Ordinary destinations therefore remain at-least-once.
#[async_trait]
pub trait DeliveryHandler: Send + Sync {
    fn retryable(&self, _error: &anyhow::Error) -> bool {
        false
    }

    async fn handle(&mut self, item: DeliveryItem<'_>) -> anyhow::Result<()>;
}

pub fn bounded_identifier(value: &str) -> anyhow::Result<()> {
    super::data::validate_identifier("delivery identity", value)?;
    anyhow::ensure!(value.len() <= 256, "delivery identity exceeds 256 bytes");
    Ok(())
}

pub fn operation_id(
    scope: &DeliveryScope,
    input: &InputEnvelope,
    candidate: &Candidate,
    ordinal: u64,
) -> anyhow::Result<DeliveryId> {
    let identity = serde_json::to_vec(&(
        "drasi.delivery.v1",
        scope,
        &input.port,
        input.envelope.system().stream(),
        &candidate.producer,
        candidate.sequence,
        ordinal,
    ))?;
    Ok(DeliveryId(format!(
        "drasi-delivery-v1-{:x}",
        Sha256::digest(identity)
    )))
}

/// Immutable, validated batch retained by a delivery boundary. Identity is
/// derived once from the actual input; operation requests cannot substitute a
/// caller-provided digest, position or idempotency key.
pub struct DeliveryBatch {
    scope: DeliveryScope,
    input: InputEnvelope,
    candidate: Candidate,
    identity: DeliveryBatchIdentity,
}
impl DeliveryBatch {
    pub fn new(
        scope: DeliveryScope,
        input: InputEnvelope,
        codec: &EnvelopeCodec,
    ) -> anyhow::Result<Self> {
        scope.validate()?;
        bounded_identifier(input.port.as_str())?;
        bounded_identifier(input.envelope.system().stream().as_str())?;
        let candidate = Candidate::new(&input.envelope, codec)?;
        let identity = batch_identity(&scope, &input, &candidate)?;
        Ok(Self {
            scope,
            input,
            candidate,
            identity,
        })
    }
    pub fn identity(&self) -> &DeliveryBatchIdentity {
        &self.identity
    }
    pub fn item(&self, index: usize) -> anyhow::Result<DeliveryItem<'_>> {
        let operation = self
            .input
            .envelope
            .changes()
            .operations()
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("delivery operation is outside its batch"))?;
        let id = operation_id(
            &self.scope,
            &self.input,
            &self.candidate,
            operation.ordinal(),
        )?;
        Ok(delivery_item(
            &self.input,
            &self.candidate,
            &self.identity,
            index,
            operation,
            id,
        ))
    }
}

pub fn delivery_item<'a>(
    input: &'a InputEnvelope,
    candidate: &Candidate,
    identity: &DeliveryBatchIdentity,
    index: usize,
    operation: &'a ChangeOperation,
    id: DeliveryId,
) -> DeliveryItem<'a> {
    DeliveryItem {
        id,
        position: DeliveryPosition {
            version: 1,
            stream: identity.stream.clone(),
            sequence: candidate.sequence,
            operation: index,
            operations: input.envelope.changes().operations().len(),
        },
        port: &input.port,
        envelope: &input.envelope,
        operation,
        content_digest: candidate.digest,
    }
}
