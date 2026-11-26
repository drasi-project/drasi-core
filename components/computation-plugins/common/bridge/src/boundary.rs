// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_contracts::ProducerSession;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AcceptanceBoundary;

/// A sealed ingress frontier, fenced to this actual receiver lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressCut {
    pub receiver_epoch: Uuid,
    pub contract: [u8; 32],
    pub boundary: AcceptanceBoundary,
    pub session: Option<ProducerSession>,
    pub client_sequence: u64,
    pub position: u64,
    /// Always empty from the receiver: a durable receiver's journal
    /// subscribers are known only to the graph that owns its admission
    /// journal, which evaluates their progress against `position`.
    pub required_subscribers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressDrainStatus {
    pub cut: IngressCut,
    /// Volatile: every accepted envelope has been returned by source.next().
    /// Durable receivers do not answer drain status; the owner of the
    /// admission journal checks its subscribers' progress instead.
    pub drained: bool,
    pub queued: usize,
    pub source_dequeued_sequence: u64,
}
