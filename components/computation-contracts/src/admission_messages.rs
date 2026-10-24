// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ComponentId, ContractError};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionRejection {
    #[error("client admission is not enabled")]
    NotEnabled,
    #[error("admission is busy")]
    Busy,
    #[error("unknown, retired or foreign producer session")]
    SessionExpired,
    #[error("producer sequence {received} is not the expected next number {expected:?}")]
    Sequence {
        received: u64,
        expected: Option<u64>,
    },
    #[error("producer sequence {0} is outside the retained receipt window")]
    ReceiptExpired(u64),
    #[error("producer sequence {0} was already accepted with a different payload")]
    PayloadConflict(u64),
    #[error("producer still has unhandled subscriber obligations")]
    PendingObligations,
    #[error("admission identifiers must not exceed 256 bytes")]
    IdentifierTooLong,
}

impl From<AdmissionRejection> for PipeError {
    fn from(error: AdmissionRejection) -> Self {
        PipeError::Backend(error.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerSession {
    pub incarnation: uuid::Uuid,
    pub producer: ComponentId,
    pub epoch: u64,
}

/// This receipt proves acceptance at the configured storage boundary, not that
/// a query or external destination has handled the event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionReceipt {
    pub session: ProducerSession,
    pub sequence: u64,
    pub position: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerStatus {
    pub session: ProducerSession,
    pub next_sequence: Option<u64>,
    pub earliest_receipt: Option<u64>,
}
