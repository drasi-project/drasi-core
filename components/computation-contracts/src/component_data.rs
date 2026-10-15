// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ChangeEnvelope, PortId};

/// Stable event identity derived from a producer stream and its sequence.
pub fn emission_id(stream: &crate::StreamId, sequence: u64) -> crate::Result<crate::EnvelopeId> {
    crate::EnvelopeId::try_new(
        stream.as_str(),
        bytes::Bytes::copy_from_slice(&sequence.to_be_bytes()),
    )
}
use async_trait::async_trait;

#[async_trait]
pub trait WakeupSource: Send + Sync {
    async fn wait(&self) -> anyhow::Result<()>;
    async fn has_pending(&self) -> anyhow::Result<bool>;
}

/// Fixed meaning of a sink's successful handle call. This is an instance-level
/// declaration, never a per-call downgrade or a consequence of pipe capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SinkCompletion {
    /// Accepted by the sink or a legacy queue. No claim of durable acceptance,
    /// completed processing, or completed external effects.
    Accepted,
    /// Processing completed at the declared sink boundary. Not automatic
    /// end-to-end acknowledgement, atomic external effects, or exactly-once.
    Handled,
}

/// Input routed to a declared input port. The host validates port/schema membership.
#[derive(Debug, Clone)]
pub struct InputEnvelope {
    pub port: PortId,
    pub envelope: ChangeEnvelope,
}

/// New emission on a declared output port. The producing component owns its
/// output stream and authoritative monotonically increasing sequence.
#[derive(Debug, Clone)]
pub struct OutputEnvelope {
    pub port: PortId,
    pub envelope: ChangeEnvelope,
}
