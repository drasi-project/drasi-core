// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use async_trait::async_trait;

use super::{ChangeEnvelope, ContractError, EnvelopeId, PipeCapabilities, PipeMetricsSnapshot};

/// Enqueue acceptance ONLY. Not downstream handling, acknowledgement, checkpoint
/// advancement, or durability (unless DurableAcceptance was explicitly negotiated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueReceipt {
    id: EnvelopeId,
}

impl EnqueueReceipt {
    /// Providers construct a receipt only after actually accepting the event.
    pub fn new(id: EnvelopeId) -> Self {
        Self { id }
    }

    pub fn envelope_id(&self) -> &EnvelopeId {
        &self.id
    }
}

/// Local handling outcome submitted separately to a delivery acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlingOutcome {
    Handled,
    Failed { reason: String },
}

/// Typed in-process pipe errors; provider errors retain their source chain.
#[derive(Debug, thiserror::Error)]
pub enum PipeError {
    #[error("bounded pipe capacity is outside the supported nonzero range")]
    InvalidCapacity,
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("pipe is closed")]
    Closed,
    #[error("pipe receiver has already been taken")]
    ReceiverTaken,
    #[error("broadcast receiver lagged by {skipped} envelopes")]
    Lagged { skipped: u64 },
    #[error("retained pipe capacity is exhausted")]
    CapacityExhausted,
    #[error("retained position {requested} is unavailable; oldest retained position is {oldest}")]
    PositionUnavailable { requested: u64, oldest: u64 },
    #[error("durable acceptance could not be determined: {source}")]
    AcceptanceUnknown {
        #[source]
        source: anyhow::Error,
    },
    #[error("durable acknowledgement could not be determined: {source}")]
    AcknowledgementUnknown {
        #[source]
        source: anyhow::Error,
    },
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

/// Failure to confirm acceptance, returning the immutable event. Volatile failures
/// are definite nonacceptance; durable commit ambiguity is explicitly distinguished.
#[derive(Debug, thiserror::Error)]
#[error("no acceptance receipt: {error}")]
pub struct SendFailure {
    pub envelope: ChangeEnvelope,
    #[source]
    pub error: PipeError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceState {
    NotAccepted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMergePolicy {
    #[default]
    Arrival,
    /// Choose the earliest currently available stream head, never reorder within
    /// a producer stream or wait for an unavailable source. Untimed heads retain
    /// arrival position and delimit groups of timed heads; this is not a watermark.
    EventTimeAcrossStreams,
}

impl SendFailure {
    pub fn acceptance(&self) -> AcceptanceState {
        if matches!(self.error, PipeError::AcceptanceUnknown { .. }) {
            AcceptanceState::Unknown
        } else {
            AcceptanceState::NotAccepted
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("batch stopped after {} accepted envelopes: {failed}", accepted.len())]
pub struct BatchSendFailure {
    pub accepted: Vec<EnqueueReceipt>,
    #[source]
    pub failed: SendFailure,
    pub not_attempted: Vec<ChangeEnvelope>,
}

/// One-shot, local-only handling acknowledgement, NOT part of a ChangeEnvelope.
/// Drop is never acknowledgement. Retry, failure, replay, and transaction scope
/// must be supplied by a future provider/host contract, not inferred here.
#[async_trait]
pub trait Acknowledgement: Send + Sync {
    /// Consume this handle to report actual handling. A successful call means
    /// the provider accepted the outcome, not an automatic end-to-end commit.
    async fn complete(
        self: Box<Self>,
        outcome: HandlingOutcome,
    ) -> std::result::Result<(), PipeError>;
}

/// Received event and optional separate local delivery capability.
/// Intentionally not Clone or serializable. A volatile pipe uses no ack handle.
pub struct Delivery {
    envelope: ChangeEnvelope,
    acknowledgement: Option<Box<dyn Acknowledgement>>,
}

impl Delivery {
    pub fn new(
        envelope: ChangeEnvelope,
        acknowledgement: Option<Box<dyn Acknowledgement>>,
    ) -> Self {
        Self {
            envelope,
            acknowledgement,
        }
    }

    pub fn envelope(&self) -> &ChangeEnvelope {
        &self.envelope
    }

    /// The host retains the handle separately while invoking a sink/transformer;
    /// neither processing success nor dropping the handle implicitly completes it.
    pub fn into_parts(self) -> (ChangeEnvelope, Option<Box<dyn Acknowledgement>>) {
        (self.envelope, self.acknowledgement)
    }
}

/// Enqueue boundary. Concurrent producers have no cross-stream global order.
/// A host serializes sends for a single producer stream in sequence order.
#[async_trait]
pub trait EnvelopeSender: Send + Sync {
    /// Sequential batch acceptance. On error, accepted receipts and the exact
    /// failed/not-attempted envelopes are retained; no batch atomicity is implied.
    async fn send_batch(
        &self,
        envelopes: Vec<ChangeEnvelope>,
    ) -> std::result::Result<Vec<EnqueueReceipt>, BatchSendFailure> {
        let mut pending = envelopes.into_iter();
        let mut accepted = Vec::new();
        for envelope in pending.by_ref() {
            match self.send(envelope).await {
                Ok(receipt) => accepted.push(receipt),
                Err(failed) => {
                    return Err(BatchSendFailure {
                        accepted,
                        failed,
                        not_attempted: pending.collect(),
                    })
                }
            }
        }
        Ok(accepted)
    }

    /// Success is acceptance only. Inspect SendFailure::acceptance before retrying:
    /// durable commit failure can be Unknown, never falsely reported as rejected.
    /// Cancellation before a result is also ambiguous.
    /// Receiver drop or runtime closure must wake blocked capacity waiters and
    /// return Closed with their unaccepted envelope, rather than hang indefinitely.
    async fn send(
        &self,
        envelope: ChangeEnvelope,
    ) -> std::result::Result<EnqueueReceipt, SendFailure>;
}

/// Single-consumer boundary. The negotiated FIFO guarantee follows producer
/// sequence, never wall-clock timestamps.
#[async_trait]
pub trait EnvelopeReceiver: Send + Sync {
    /// None means closed and drained, not temporarily empty. Cancellation must
    /// not silently acknowledge or discard an event removed from a volatile queue;
    /// durable providers retain responsibility until their negotiated completion.
    /// Dropping all senders or runtime closure must wake a waiting receiver;
    /// drain accepted events before returning None on a graceful close.
    async fn receive(&mut self) -> std::result::Result<Option<Delivery>, PipeError>;
}

/// In-process pipe interface. The graph validates capabilities/requirements
/// before starting components; [`super::BoundedPipe`] supplies volatile delivery.
pub trait Pipe: Send + Sync {
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        None
    }
    fn capabilities(&self) -> &PipeCapabilities;
    fn sender(&self) -> Arc<dyn EnvelopeSender>;
    /// Transfer the single receiver to its owning task; subsequent calls fail.
    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError>;
}
