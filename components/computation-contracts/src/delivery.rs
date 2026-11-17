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

/// A consumer's configured delivery guarantee.
///
/// `ExactlyOnce` (the default) accepts only producers with a durable output
/// identity. `AtLeastOnce` additionally accepts producers with a volatile
/// identity (in-memory queries and volatile graph producers): their delivery
/// carries an epoch, deduplicates within one producer incarnation, and cannot
/// resume across a process restart. A consumer declares its guarantee as its
/// inputs' [`crate::InputDelivery`] in its [`crate::ComponentDescription`], and
/// the graph checks it against the producer's declared output when the edge is
/// bound ([`crate::check_edge`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryGuarantee {
    #[default]
    ExactlyOnce,
    AtLeastOnce,
}

impl DeliveryGuarantee {
    pub fn is_at_least_once(self) -> bool {
        self == Self::AtLeastOnce
    }
}

/// Epochs are lowercase SHA-256 hex digests.
pub const DELIVERY_EPOCH_LEN: usize = 64;

fn valid_epoch(epoch: &str) -> bool {
    epoch.len() == DELIVERY_EPOCH_LEN
        && epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether a destination cursor's stored epoch column is valid: empty for an
/// exactly-once stream, otherwise a delivery epoch.
pub fn valid_cursor_epoch(epoch: &str) -> bool {
    epoch.is_empty() || valid_epoch(epoch)
}

/// How an incoming item relates to its stream's stored destination cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorEpoch {
    /// Same epoch (or both exactly-once): the ordinary cursor protocol applies.
    Same,
    /// An at-least-once item from a new producer epoch: the stored row is
    /// treated as absent and overwritten (a fresh cursor for the new run).
    New,
}

/// The shared destination-cursor epoch rule. `stored` and `incoming` are the
/// row's and the item's epochs, empty for exactly-once. An exactly-once item
/// never matches an at-least-once row, nor the reverse.
pub fn cursor_epoch(stored: &str, incoming: &str) -> Result<CursorEpoch, &'static str> {
    if stored == incoming {
        Ok(CursorEpoch::Same)
    } else if stored.is_empty() || incoming.is_empty() {
        Err("destination delivery cursor belongs to a stream with a different delivery guarantee")
    } else {
        Ok(CursorEpoch::New)
    }
}

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
    /// Version 2 only: the volatile producer epoch of an at-least-once batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch: Option<String>,
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

    /// At-least-once counterpart of [`Self::for_input`] for an immediate
    /// in-memory query output (see [`Candidate::new_volatile`]).
    pub fn for_input_at_least_once(
        scope: &DeliveryScope,
        input: &InputEnvelope,
        codec: &EnvelopeCodec,
    ) -> anyhow::Result<Self> {
        scope.validate()?;
        bounded_identifier(input.port.as_str())?;
        bounded_identifier(input.envelope.system().stream().as_str())?;
        batch_identity(
            scope,
            input,
            &Candidate::new_volatile(&input.envelope, codec)?,
        )
    }

    /// The volatile producer epoch; `None` for exactly-once batches.
    pub fn epoch(&self) -> Option<&str> {
        self.epoch.as_deref()
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
            version: self.position.version,
            stream: self.position.stream.clone(),
            sequence: self.position.sequence,
            operations: self.position.operations,
            digest: self.content_digest,
            epoch: self.position.epoch.clone(),
        })
    }
}

pub fn batch_identity(
    scope: &DeliveryScope,
    input: &InputEnvelope,
    candidate: &Candidate,
) -> anyhow::Result<DeliveryBatchIdentity> {
    let stream = match (&candidate.epoch, &candidate.producer) {
        (None, producer) => serde_json::to_vec(&(
            "drasi.delivery-stream.v1",
            scope,
            &input.port,
            input.envelope.system().stream(),
            producer,
        ))?,
        // Volatile producers: the key excludes the incarnation, generation and
        // configuration (all carried by the epoch), so one destination cursor
        // serves every run instead of accumulating per process.
        (Some(epoch), crate::output_identity::Producer::Query { identity, .. }) => {
            anyhow::ensure!(valid_epoch(epoch), "invalid delivery epoch");
            serde_json::to_vec(&(
                "drasi.delivery-stream.volatile-query.v1",
                scope,
                &input.port,
                input.envelope.system().stream(),
                identity.construction_scope(),
                identity.graph_id(),
                identity.query_id(),
            ))?
        }
        (Some(epoch), crate::output_identity::Producer::Graph(identity)) => {
            anyhow::ensure!(valid_epoch(epoch), "invalid delivery epoch");
            serde_json::to_vec(&(
                "drasi.delivery-stream.volatile-graph.v1",
                scope,
                &input.port,
                input.envelope.system().stream(),
                identity.construction_scope(),
                identity.graph_id(),
                identity.component_id(),
            ))?
        }
    };
    Ok(DeliveryBatchIdentity {
        version: if candidate.epoch.is_some() { 2 } else { 1 },
        stream: format!("drasi-delivery-stream-v1-{:x}", Sha256::digest(stream)),
        sequence: candidate.sequence,
        operations: input.envelope.changes().operations().len(),
        digest: candidate.digest,
        epoch: candidate.epoch.clone(),
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
    /// Version 2 only: the volatile producer epoch of an at-least-once item.
    /// A destination cursor whose stored epoch differs treats its row as
    /// absent; strict (version 1) positions never carry an epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch: Option<String>,
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
    /// The volatile producer epoch; `None` for exactly-once delivery.
    pub fn epoch(&self) -> Option<&str> {
        self.epoch.as_deref()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        const PREFIX: &str = "drasi-delivery-stream-v1-";
        anyhow::ensure!(
            match (self.version, &self.epoch) {
                (1, None) => true,
                (2, Some(epoch)) => valid_epoch(epoch),
                _ => false,
            } && self.sequence > 0
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

    /// The handler's configured guarantee. Only an `AtLeastOnce` handler may
    /// receive items carrying a volatile producer epoch.
    fn delivery_guarantee(&self) -> DeliveryGuarantee {
        DeliveryGuarantee::ExactlyOnce
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
        Self::assemble(scope, input, candidate)
    }

    /// Derive a batch under the consumer's configured guarantee. Strict
    /// derivation is always tried first; only its missing-progress rejection of
    /// an output with a volatile producer identity falls back to the
    /// at-least-once identity, and only for an `AtLeastOnce` consumer.
    ///
    /// The graph refuses incompatible producers when the edge is bound
    /// ([`crate::check_edge`]); the permanent
    /// [`crate::Incompatibility::ProducerIdentity`] error here is only an
    /// internal invariant guard, never the compatibility check.
    pub fn new_with_guarantee(
        scope: DeliveryScope,
        input: InputEnvelope,
        codec: &EnvelopeCodec,
        guarantee: DeliveryGuarantee,
    ) -> anyhow::Result<Self> {
        scope.validate()?;
        bounded_identifier(input.port.as_str())?;
        bounded_identifier(input.envelope.system().stream().as_str())?;
        let candidate = match Candidate::new(&input.envelope, codec) {
            Ok(candidate) => candidate,
            Err(error) if crate::output_identity::is_missing_progress(&error) => {
                let Ok(candidate) = Candidate::new_volatile(&input.envelope, codec) else {
                    return Err(error);
                };
                if !guarantee.is_at_least_once() {
                    return Err(crate::Incompatibility::ProducerIdentity {
                        required: DeliveryGuarantee::ExactlyOnce,
                        identity: crate::OutputIdentity::Volatile,
                    }
                    .into());
                }
                candidate
            }
            Err(error) => return Err(error),
        };
        Self::assemble(scope, input, candidate)
    }

    fn assemble(
        scope: DeliveryScope,
        input: InputEnvelope,
        candidate: Candidate,
    ) -> anyhow::Result<Self> {
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
            version: if candidate.epoch.is_some() { 2 } else { 1 },
            stream: identity.stream.clone(),
            sequence: candidate.sequence,
            operation: index,
            operations: input.envelope.changes().operations().len(),
            epoch: candidate.epoch.clone(),
        },
        port: &input.port,
        envelope: &input.envelope,
        operation,
        content_digest: candidate.digest,
    }
}
