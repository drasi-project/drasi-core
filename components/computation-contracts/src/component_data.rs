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

/// Opt-in host collection limits. A single envelope is never split by the host;
/// `max_operations` is a flush target, while `max_inputs` is a hard count bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkBatching {
    pub max_inputs: usize,
    pub max_operations: usize,
    pub max_bytes: usize,
    pub max_delay_ms: u64,
}

impl SinkBatching {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=1024).contains(&self.max_inputs)
                && (1..=65_536).contains(&self.max_operations)
                && (1..=64 * 1024 * 1024).contains(&self.max_bytes)
                && (1..=60_000).contains(&self.max_delay_ms),
            "sink batching requires 1..=1024 inputs, 1..=65536 operations, 1..=67108864 bytes and 1..=60000ms"
        );
        Ok(())
    }

    pub fn validate_inputs(&self, inputs: &[InputEnvelope]) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            !inputs.is_empty() && inputs.len() <= self.max_inputs,
            "sink batch is empty or exceeds its input limit"
        );
        anyhow::ensure!(
            inputs.iter().all(|input| input.port == inputs[0].port)
                && (inputs.len() == 1 || !inputs.iter().any(Self::is_barrier)),
            "sink batch crosses an input port or control/snapshot/progress barrier"
        );
        let bytes = inputs.iter().try_fold(5usize, |bytes, input| {
            bytes
                .checked_add(Self::input_size(input)?)
                .ok_or_else(|| anyhow::anyhow!("sink batch byte count overflow"))
        })?;
        anyhow::ensure!(bytes <= self.max_bytes, "sink batch exceeds its byte limit");
        Ok(())
    }

    /// Conservative size of the SDK's named MessagePack input frame, including
    /// its port and binary-envelope framing. Five more bytes bound the batch array.
    pub fn input_size(input: &InputEnvelope) -> anyhow::Result<usize> {
        crate::BinaryEnvelopeCodec::encoded_size(&input.envelope)?
            .checked_add(input.port.as_str().len())
            .and_then(|size| size.checked_add(25))
            .ok_or_else(|| anyhow::anyhow!("sink input byte count overflow"))
    }

    pub fn is_barrier(input: &InputEnvelope) -> bool {
        input.envelope.changes().operations().is_empty()
            || crate::QueryChangeCodec::is_snapshot(&input.envelope)
            || crate::QueryChangeCodec::is_progress_only(&input.envelope)
    }
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
