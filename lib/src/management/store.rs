// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::DesiredInstance;

/// A durably accepted desired definition, not a checkpoint of running objects.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedConfiguration {
    pub revision: u64,
    pub desired: DesiredInstance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceReceipt {
    pub request_id: String,
    pub revision: u64,
    pub durable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagementError {
    #[error("configuration revision conflict: expected {expected}, current {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("request ID was already used for a different desired configuration")]
    RequestConflict,
    #[error("instance {0} already has a configuration owner")]
    AlreadyOwned(String),
    #[error("configuration store session is closed")]
    Closed,
    #[error("configuration snapshot {0} already exists")]
    SnapshotExists(String),
}

/// External provider for authoritative configuration. Core has no database or
/// dynamic-library dependency. Implementations must protect credential-bearing
/// records, including journals, receipts and snapshots, at rest.
#[async_trait]
pub trait ConfigurationStore: Send + Sync {
    /// Acquire exclusive management ownership for this instance. Different
    /// instance IDs may share the same physical database.
    async fn open(&self, instance_id: &str) -> anyhow::Result<Arc<dyn ConfigurationSession>>;
}

/// Owned store session. All calls must be safe on a current-thread runtime.
/// A cancelled caller must not make a committed request ambiguous: commit and
/// receipt storage are atomic, and callers can resolve by request ID.
#[async_trait]
pub trait ConfigurationSession: Send + Sync {
    async fn load(&self) -> anyhow::Result<CommittedConfiguration>;

    /// Atomically check the revision, store desired state and its idempotency
    /// receipt, and wait for durable completion. Identical request IDs and
    /// definitions return the original receipt even if expected_revision is old.
    /// Changed payload under an existing request ID must fail. Identical current
    /// desired state may retain its revision but still records the new request.
    async fn commit(
        &self,
        expected_revision: u64,
        request_id: &str,
        desired: &DesiredInstance,
    ) -> anyhow::Result<AcceptanceReceipt>;

    async fn receipt(&self, request_id: &str) -> anyhow::Result<Option<AcceptanceReceipt>>;

    /// Store an immutable snapshot of the current committed revision atomically
    /// with reading it. Snapshots are privileged configuration, not data backups.
    async fn snapshot(&self, name: &str) -> anyhow::Result<CommittedConfiguration>;
    async fn load_snapshot(&self, name: &str) -> anyhow::Result<Option<CommittedConfiguration>>;

    /// Reject new calls, finish any admitted storage work, then release ownership.
    async fn close(&self) -> anyhow::Result<()>;
}
