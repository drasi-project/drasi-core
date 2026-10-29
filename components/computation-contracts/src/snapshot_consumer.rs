// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional, input-port-scoped replacement of a consumer's state from a snapshot.

use async_trait::async_trait;

use crate::InputEnvelope;

/// A sink input's ability to replace its state from an explicit snapshot
/// envelope. A sink returns this only for input ports whose registered handler
/// has real replacement behaviour; absence is expressed by returning no handler,
/// never by a succeeding or unsupported implementation.
///
/// The host owns serialization with other calls on the same sink. Success means
/// the replacement was applied to the consumer's state; failure leaves the
/// consumer to the host's recovery policy.
#[async_trait]
pub trait SnapshotConsumer: Send + Sync {
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> anyhow::Result<()>;
}
