// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Opt-in, ordered operation completion. Transactional business state shares the
//! progress commit. External effects need destination-side atomic deduplication.

mod retirement;
pub(crate) use retirement::DeliveryRetirement;
pub use retirement::{ConsumerDeliveryResource, DeliveryRetirementState};

use std::{collections::VecDeque, num::NonZeroUsize, time::Duration};

use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexes, ComputationQueryError, ComputationTransaction},
    interface::IndexError,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    output_identity::{Candidate, Producer},
    ChangeEnvelope, ChangeOperation, ComponentId, EnvelopeCodec, InputEnvelope, PortId,
    RecoveryScope, ReplayRejection, StreamId, TransactionContext,
};

const METADATA_KEY: &str = "computation:delivery:v1";
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryScope {
    construction_scope: String,
    graph: String,
    consumer: ComponentId,
}

impl DeliveryScope {
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

    fn validate(&self) -> anyhow::Result<()> {
        for value in [&self.construction_scope, &self.graph, self.consumer.as_str()] {
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

/// Retries per operation in one deliver call, including the first attempt.
/// Only errors explicitly classified by the handler are retried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryRetryPolicy {
    pub max_attempts: NonZeroUsize,
    pub delay: Duration,
}

impl Default for DeliveryRetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: NonZeroUsize::MIN,
            delay: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryOptions {
    pub scope: RecoveryScope,
    pub max_streams: NonZeroUsize,
    pub receipts_per_stream: NonZeroUsize,
    pub retry: DeliveryRetryPolicy,
}

impl DeliveryOptions {
    /// Validate bounds before acquiring delivery storage or accepting a recipe.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_streams.get() <= 256
                && self.receipts_per_stream.get() <= 1024
                && self.max_streams.get() * self.receipts_per_stream.get() <= 4096,
            "delivery progress supports at most 256 streams and 4096 total receipts"
        );
        anyhow::ensure!(
            self.retry.max_attempts.get() <= 32 && self.retry.delay <= Duration::from_secs(60),
            "delivery retry policy supports at most 32 attempts and 60 seconds between attempts"
        );
        Ok(())
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

fn batch_identity(
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

/// Business state and handled identity commit in the runner's one transaction.
/// Use only the borrowed context for mutable business state: no external effects,
/// nested transactions, independent writes, workers, or commit-sensitive mutable
/// fields on the handler. Atomicity is per operation, not the entire input batch.
#[async_trait]
pub trait TransactionalDeliveryHandler: Send + Sync {
    fn retryable(&self, _error: &anyhow::Error) -> bool {
        false
    }

    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        context: &TransactionContext<'_>,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryMode {
    #[default]
    External,
    Transactional,
}

impl DeliveryMode {
    fn is_external(&self) -> bool {
        *self == Self::External
    }
}

enum DeliveryTarget<'a> {
    External(&'a mut dyn DeliveryHandler),
    Transactional(&'a dyn TransactionalDeliveryHandler),
}

impl DeliveryTarget<'_> {
    fn mode(&self) -> DeliveryMode {
        match self {
            Self::External(_) => DeliveryMode::External,
            Self::Transactional(_) => DeliveryMode::Transactional,
        }
    }

    fn retryable(&self, error: &anyhow::Error) -> bool {
        match self {
            Self::External(handler) => handler.retryable(error),
            Self::Transactional(handler) => handler.retryable(error),
        }
    }
}

enum DeliveryAttempt {
    Complete,
    Failed(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("delivery ledger is invalid: {0}")]
    InvalidLedger(String),
    #[error("delivery ledger cannot be decoded: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("delivery ledger belongs to another consumer")]
    ScopeMismatch,
    #[error("delivery handling mode differs from the runner or persisted progress")]
    ModeMismatch,
    #[error("delivery stream capacity is exhausted")]
    StreamCapacity,
    #[error("logical output {sequence} still has unfinished operations")]
    Pending { sequence: u64 },
    #[error("operation {id} did not complete after {attempts} attempt(s): {source}")]
    Handling {
        id: DeliveryId,
        attempts: usize,
        #[source]
        source: anyhow::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    sequence: u64,
    digest: [u8; 32],
    operations: usize,
    completed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamProgress {
    port: PortId,
    stream: StreamId,
    producer: Producer,
    handled: u64,
    receipts: VecDeque<Receipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeliveryProgress {
    pub port: PortId,
    pub stream: StreamId,
    pub handled_sequence: u64,
    pub latest_sequence: u64,
    pub completed_operations: usize,
    pub operations: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    scope: DeliveryScope,
    #[serde(default, skip_serializing_if = "DeliveryMode::is_external")]
    mode: DeliveryMode,
    streams: Vec<StreamProgress>,
}

fn bounded_identifier(value: &str) -> anyhow::Result<()> {
    super::data::validate_identifier("delivery identity", value)?;
    anyhow::ensure!(value.len() <= 256, "delivery identity exceeds 256 bytes");
    Ok(())
}

fn operation_id(
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

fn delivery_item<'a>(
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

impl Ledger {
    fn validate(
        &self,
        scope: &DeliveryScope,
        options: &DeliveryOptions,
        mode: DeliveryMode,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported delivery ledger version");
        self.scope.validate()?;
        if self.scope != *scope {
            return Err(DeliveryError::ScopeMismatch.into());
        }
        if self.mode != mode {
            return Err(DeliveryError::ModeMismatch.into());
        }
        anyhow::ensure!(
            self.streams.len() <= options.max_streams.get(),
            "too many delivery streams"
        );
        let mut streams = std::collections::BTreeSet::new();
        for stream in &self.streams {
            bounded_identifier(stream.port.as_str())?;
            bounded_identifier(stream.stream.as_str())?;
            stream.producer.validate(&stream.stream)?;
            anyhow::ensure!(
                streams.insert((&stream.port, &stream.stream))
                    && !stream.receipts.is_empty()
                    && stream.receipts.len() <= options.receipts_per_stream.get(),
                "invalid delivery stream or receipt count"
            );
            let mut previous = 0;
            for (index, receipt) in stream.receipts.iter().enumerate() {
                anyhow::ensure!(
                    receipt.sequence > previous
                        && receipt.completed <= receipt.operations
                        && (receipt.completed == receipt.operations
                            || index + 1 == stream.receipts.len()),
                    "invalid delivery receipt order or completion"
                );
                previous = receipt.sequence;
            }
            let last = stream.receipts.back().expect("nonempty receipts");
            anyhow::ensure!(
                if last.completed == last.operations {
                    stream.handled == last.sequence
                } else {
                    stream.handled < last.sequence
                        && stream
                            .receipts
                            .iter()
                            .rev()
                            .nth(1)
                            .map_or(true, |previous| previous.sequence == stream.handled)
                },
                "delivery handled cursor disagrees with receipts"
            );
        }
        Ok(())
    }

    fn admit(
        &mut self,
        input: &InputEnvelope,
        candidate: &Candidate,
        options: &DeliveryOptions,
    ) -> anyhow::Result<(usize, usize, bool)> {
        let found = self.streams.iter().position(|stream| {
            stream.port == input.port && stream.stream == *input.envelope.system().stream()
        });
        let index = match found {
            Some(index) => index,
            None => {
                if self.streams.len() == options.max_streams.get() {
                    return Err(DeliveryError::StreamCapacity.into());
                }
                self.streams.push(StreamProgress {
                    port: input.port.clone(),
                    stream: input.envelope.system().stream().clone(),
                    producer: candidate.producer.clone(),
                    handled: 0,
                    receipts: VecDeque::new(),
                });
                self.streams.len() - 1
            }
        };
        let stream = &mut self.streams[index];
        if stream.producer != candidate.producer {
            return Err(ReplayRejection::ProducerChanged.into());
        }
        if let Some(receipt) = stream
            .receipts
            .iter()
            .find(|receipt| receipt.sequence == candidate.sequence)
        {
            if receipt.digest != candidate.digest
                || receipt.operations != input.envelope.changes().operations().len()
            {
                return Err(ReplayRejection::PayloadConflict(candidate.sequence).into());
            }
            return Ok((index, receipt.completed, false));
        }
        if let Some(last) = stream.receipts.back() {
            if candidate.sequence <= last.sequence {
                return Err(ReplayRejection::ReceiptExpired(candidate.sequence).into());
            }
            if last.completed != last.operations {
                return Err(DeliveryError::Pending {
                    sequence: last.sequence,
                }
                .into());
            }
        }
        if stream.receipts.len() == options.receipts_per_stream.get() {
            stream.receipts.pop_front();
        }
        stream.receipts.push_back(Receipt {
            sequence: candidate.sequence,
            digest: candidate.digest,
            operations: input.envelope.changes().operations().len(),
            completed: 0,
        });
        if input.envelope.changes().operations().is_empty() {
            stream.handled = candidate.sequence;
        }
        Ok((index, 0, true))
    }
}

/// One explicitly owned progress ledger over a dedicated atomic index bundle.
/// It never retains input payloads: an upstream lossless journal must keep input
/// until deliver succeeds. No worker, retry, hashing or storage is added to
/// ordinary sinks. Serialize calls and await shutdown before opening a replacement.
pub struct DeliveryRunner {
    transaction: ComputationTransaction,
    scope: DeliveryScope,
    codec: EnvelopeCodec,
    options: DeliveryOptions,
    mode: DeliveryMode,
    ledger: Option<Ledger>,
}

impl DeliveryRunner {
    pub fn new(
        scope: DeliveryScope,
        indexes: ComputationIndexes,
        codec: EnvelopeCodec,
        options: DeliveryOptions,
    ) -> anyhow::Result<Self> {
        scope.validate()?;
        options.validate()?;
        anyhow::ensure!(
            options.scope.survives(indexes.durability())
                == drasi_core::interface::FailureSurvival::Guaranteed,
            "delivery progress storage does not satisfy the requested failure scope"
        );
        anyhow::ensure!(
            indexes.cleanup().is_some(),
            "delivery progress needs an asynchronous cleanup owner"
        );
        Ok(Self {
            transaction: ComputationTransaction::try_new(indexes)?,
            scope,
            codec,
            options,
            mode: DeliveryMode::External,
            ledger: None,
        })
    }

    /// Commit context state and each operation's completion together. Persisted
    /// external progress cannot be reinterpreted as transactional-state progress.
    pub fn new_transactional(
        scope: DeliveryScope,
        indexes: ComputationIndexes,
        codec: EnvelopeCodec,
        options: DeliveryOptions,
    ) -> anyhow::Result<Self> {
        let mut runner = Self::new(scope, indexes, codec, options)?;
        runner.mode = DeliveryMode::Transactional;
        Ok(runner)
    }

    pub fn uses_transactional_state(&self) -> bool {
        self.mode == DeliveryMode::Transactional
    }

    async fn load(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.transaction.recovery_required(),
            "delivery progress requires cleanup and reconstruction"
        );
        if self.ledger.is_some() {
            return Ok(());
        }
        let outbox = self
            .transaction
            .resources()
            .outbox_writer()
            .expect("validated outbox");
        let (head, records) = self
            .transaction
            .run(async {
                let head = outbox.read_latest_sequence(METADATA_KEY).await?;
                let records = if head == Some(1) {
                    outbox.read_from(METADATA_KEY, 0).await?
                } else {
                    Vec::new()
                };
                Ok((head, records))
            })
            .await?;
        let ledger = match (head, records.as_slice()) {
            (None, []) => Ledger {
                version: 1,
                scope: self.scope.clone(),
                mode: self.mode,
                streams: Vec::new(),
            },
            (Some(1), [(1, bytes)]) if bytes.len() <= MAX_METADATA_BYTES => {
                serde_json::from_slice(bytes).map_err(DeliveryError::Decode)?
            }
            _ => {
                return Err(DeliveryError::InvalidLedger(
                    "expected one bounded metadata row at sequence one".into(),
                )
                .into())
            }
        };
        ledger.validate(&self.scope, &self.options, self.mode)?;
        self.ledger = Some(ledger);
        Ok(())
    }

    fn encode_ledger(&self, ledger: &Ledger) -> anyhow::Result<Vec<u8>> {
        ledger.validate(&self.scope, &self.options, self.mode)?;
        let bytes = serde_json::to_vec(ledger)?;
        anyhow::ensure!(
            bytes.len() <= MAX_METADATA_BYTES,
            "delivery ledger exceeds four MiB"
        );
        Ok(bytes)
    }

    async fn save(&mut self, ledger: Ledger) -> anyhow::Result<()> {
        let bytes = self.encode_ledger(&ledger)?;
        let outbox = self
            .transaction
            .resources()
            .outbox_writer()
            .expect("validated outbox");
        self.transaction
            .run(async {
                outbox.append(METADATA_KEY, 1, &bytes).await?;
                Ok(())
            })
            .await?;
        self.ledger = Some(ledger);
        Ok(())
    }

    pub async fn deliver(
        &mut self,
        input: &InputEnvelope,
        handler: &mut dyn DeliveryHandler,
    ) -> anyhow::Result<()> {
        self.deliver_to(input, DeliveryTarget::External(handler))
            .await
    }

    pub async fn deliver_transactional(
        &mut self,
        input: &InputEnvelope,
        handler: &dyn TransactionalDeliveryHandler,
    ) -> anyhow::Result<()> {
        self.deliver_to(input, DeliveryTarget::Transactional(handler))
            .await
    }

    async fn transactional_attempt(
        &self,
        item: DeliveryItem<'_>,
        handler: &dyn TransactionalDeliveryHandler,
        ledger: &Ledger,
        attempt: usize,
    ) -> anyhow::Result<DeliveryAttempt> {
        let bytes = self.encode_ledger(ledger)?;
        let resources = self.transaction.resources();
        let context = TransactionContext::new(
            &self.scope.consumer,
            resources.indexes().element_index.as_ref(),
            StreamId::try_new(item.position.stream.clone())?,
            item.position.sequence,
        );
        let outbox = resources.outbox_writer().expect("validated outbox");
        let id = item.id.clone();
        let failed = |source| {
            IndexError::other(DeliveryError::Handling {
                id: id.clone(),
                attempts: attempt,
                source,
            })
        };
        let result = self
            .transaction
            .run(async {
                handler.handle(item, &context).await.map_err(failed)?;
                context.validate().map_err(failed)?;
                outbox.append(METADATA_KEY, 1, &bytes).await?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) => Ok(DeliveryAttempt::Complete),
            Err(ComputationQueryError::Index(IndexError::Other(error)))
                if !self.transaction.recovery_required() =>
            {
                match error.downcast::<DeliveryError>() {
                    Ok(error) => match *error {
                        DeliveryError::Handling { source, .. } => {
                            Ok(DeliveryAttempt::Failed(source))
                        }
                        error => Err(ComputationQueryError::Index(IndexError::other(error)).into()),
                    },
                    Err(error) => {
                        Err(ComputationQueryError::Index(IndexError::Other(error)).into())
                    }
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn deliver_to(
        &mut self,
        input: &InputEnvelope,
        mut target: DeliveryTarget<'_>,
    ) -> anyhow::Result<()> {
        if self.mode != target.mode() {
            return Err(DeliveryError::ModeMismatch.into());
        }
        bounded_identifier(input.port.as_str())?;
        bounded_identifier(input.envelope.system().stream().as_str())?;
        let candidate = Candidate::new(&input.envelope, &self.codec)?;
        self.load().await?;
        let mut ledger = self.ledger.as_ref().expect("loaded ledger").clone();
        let (stream, completed, added) = ledger.admit(input, &candidate, &self.options)?;
        // Record identity and exact content before any destination can act.
        if added {
            self.save(ledger).await?;
        }
        let identity = batch_identity(&self.scope, input, &candidate)?;
        for (index, operation) in input
            .envelope
            .changes()
            .operations()
            .iter()
            .enumerate()
            .skip(completed)
        {
            let id = operation_id(&self.scope, input, &candidate, operation.ordinal())?;
            let mut next = self.ledger.as_ref().expect("loaded ledger").clone();
            let progress = &mut next.streams[stream];
            let receipt = progress.receipts.back_mut().expect("admitted receipt");
            receipt.completed += 1;
            if receipt.completed == receipt.operations {
                progress.handled = receipt.sequence;
            }
            for attempt in 1..=self.options.retry.max_attempts.get() {
                let item =
                    delivery_item(input, &candidate, &identity, index, operation, id.clone());
                let outcome = match &mut target {
                    DeliveryTarget::External(handler) => match handler.handle(item).await {
                        Ok(()) => DeliveryAttempt::Complete,
                        Err(error) => DeliveryAttempt::Failed(error),
                    },
                    DeliveryTarget::Transactional(handler) => {
                        self.transactional_attempt(item, *handler, &next, attempt)
                            .await?
                    }
                };
                match outcome {
                    DeliveryAttempt::Complete => break,
                    DeliveryAttempt::Failed(source) => {
                        if attempt == self.options.retry.max_attempts.get()
                            || !target.retryable(&source)
                        {
                            return Err(DeliveryError::Handling {
                                id,
                                attempts: attempt,
                                source,
                            }
                            .into());
                        }
                        log::warn!("Retrying delivery {id} after attempt {attempt}: {source:#}");
                        tokio::time::sleep(self.options.retry.delay).await;
                    }
                }
            }
            match self.mode {
                DeliveryMode::External => self.save(next).await?,
                DeliveryMode::Transactional => self.ledger = Some(next),
            }
        }
        Ok(())
    }

    /// Validate and identify a batch without changing progress or invoking effects.
    pub fn batch_identity(&self, input: &InputEnvelope) -> anyhow::Result<DeliveryBatchIdentity> {
        bounded_identifier(input.port.as_str())?;
        bounded_identifier(input.envelope.system().stream().as_str())?;
        batch_identity(
            &self.scope,
            input,
            &Candidate::new(&input.envelope, &self.codec)?,
        )
    }

    pub async fn progress(&mut self) -> anyhow::Result<Vec<DeliveryProgress>> {
        self.load().await?;
        Ok(self
            .ledger
            .as_ref()
            .expect("loaded ledger")
            .streams
            .iter()
            .map(|stream| {
                let latest = stream.receipts.back().expect("nonempty receipts");
                DeliveryProgress {
                    port: stream.port.clone(),
                    stream: stream.stream.clone(),
                    handled_sequence: stream.handled,
                    latest_sequence: latest.sequence,
                    completed_operations: latest.completed,
                    operations: latest.operations,
                }
            })
            .collect())
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.transaction.shutdown().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
