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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationSnapshotSummary {
    pub name: String,
    pub revision: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagementError {
    #[error("configuration state could not be confirmed; reconcile before reading authoritative desired state")]
    ConfigurationUnconfirmed,
    #[error("configuration revision conflict: expected {expected}, current {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("request ID was already used for a different desired configuration")]
    RequestConflict,
    #[error("configuration request has expired; inspect current configuration before creating a new request")]
    RequestExpired,
    #[error("this store requires an ID from new_configuration_request_id or ConfigurationSession::new_request_id")]
    GeneratedRequestIdRequired,
    #[error(
        "configuration request batch is full; obtain a new request ID before submitting new work"
    )]
    RequestBatchFull,
    #[error("configuration store does not support {0}")]
    UnsupportedOperation(&'static str),
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
/// Commit and receipt storage are atomic, even if the caller cancels. Callers can
/// resolve by request ID until an explicitly configured retry window expires.
#[async_trait]
pub trait ConfigurationSession: Send + Sync {
    async fn load(&self) -> anyhow::Result<CommittedConfiguration>;

    /// Generate an ID for a new operation, never for a retry. Expiring stores
    /// may advance their persisted batch and expire old IDs when the batch is full.
    /// A generated but unused ID may also expire. Expiry does not prove whether
    /// an old operation committed; inspect current state instead of reapplying it.
    async fn new_request_id(&self) -> anyhow::Result<String> {
        self.load().await?;
        Ok(uuid::Uuid::new_v4().to_string())
    }

    /// Atomically check the revision, store desired state and its idempotency
    /// receipt, and wait for durable completion. Identical request IDs and
    /// definitions return the original receipt even if expected_revision is old.
    /// Changed payload under an existing request ID must fail. Identical current
    /// desired state may retain its revision but still records the new request.
    /// The default contract retains receipts indefinitely. An explicitly opted-in
    /// expiry mode must require generated, persisted-batch-scoped IDs and reject
    /// expired IDs in both commit and receipt lookup, never treat them as new.
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

    /// Bounded metadata listing. `after` is the last returned name, used as a
    /// provider-order cursor, not a cross-call snapshot.
    async fn list_snapshots(
        &self,
        _after: Option<&str>,
        _limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<Vec<ConfigurationSnapshotSummary>> {
        Err(ManagementError::UnsupportedOperation("snapshot listing").into())
    }

    /// Delete only the named immutable snapshot at the expected snapshot revision.
    /// Missing snapshots return false; a different revision is a conflict.
    /// Does not change current configuration or processing state.
    async fn delete_snapshot(&self, _name: &str, _expected_revision: u64) -> anyhow::Result<bool> {
        Err(ManagementError::UnsupportedOperation("snapshot deletion").into())
    }

    /// Reject new calls, finish any admitted storage work, then release ownership.
    async fn close(&self) -> anyhow::Result<()>;
}
