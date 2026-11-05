// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! External configuration-store ownership, acceptance and snapshot contracts.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::DesiredInstance;

/// A durably accepted desired definition, not a checkpoint of running objects.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedConfiguration {
    /// Revision of the durably accepted desired configuration.
    pub revision: u64,
    /// Desired definitions, not the state of running components.
    pub desired: DesiredInstance,
}

/// Idempotent acceptance of one desired-configuration request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceReceipt {
    /// Request identity used to resolve retries.
    pub request_id: String,
    /// Accepted desired-configuration revision.
    pub revision: u64,
    /// Whether acceptance has the store's promised durability.
    pub durable: bool,
}

/// Name and immutable revision of a saved configuration snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationSnapshotSummary {
    /// Snapshot name within the instance's configuration session.
    pub name: String,
    /// Revision captured when the snapshot was created.
    pub revision: u64,
}

/// Explicit configuration ownership, acceptance and snapshot failures.
#[derive(Debug, thiserror::Error)]
pub enum ManagementError {
    /// A possibly accepted write must be reconciled before authoritative reads.
    #[error("configuration state could not be confirmed; reconcile before reading authoritative desired state")]
    ConfigurationUnconfirmed,
    /// The caller's expected revision is no longer current.
    #[error("configuration revision conflict: expected {expected}, current {actual}")]
    RevisionConflict {
        /// Revision supplied by the caller.
        expected: u64,
        /// Revision currently owned by the store.
        actual: u64,
    },
    /// A request ID was reused for a different desired definition.
    #[error("request ID was already used for a different desired configuration")]
    RequestConflict,
    /// The retry identity expired and cannot be submitted as new work.
    #[error("configuration request has expired; inspect current configuration before creating a new request")]
    RequestExpired,
    /// The store requires an ID generated through its current request batch.
    #[error("this store requires an ID from new_configuration_request_id or ConfigurationSession::new_request_id")]
    GeneratedRequestIdRequired,
    /// The current request batch cannot admit another new operation.
    #[error(
        "configuration request batch is full; obtain a new request ID before submitting new work"
    )]
    RequestBatchFull,
    /// The provider does not implement the named optional operation.
    #[error("configuration store does not support {0}")]
    UnsupportedOperation(&'static str),
    /// Another session already owns the named instance.
    #[error("instance {0} already has a configuration owner")]
    AlreadyOwned(String),
    /// This configuration session no longer admits operations.
    #[error("configuration store session is closed")]
    Closed,
    /// An immutable snapshot already uses the requested name.
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
    /// Read the last confirmed committed configuration.
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

    /// Resolve a request without reapplying it; expired identities remain errors.
    async fn receipt(&self, request_id: &str) -> anyhow::Result<Option<AcceptanceReceipt>>;

    /// Store an immutable snapshot of the current committed revision atomically
    /// with reading it. Snapshots are privileged configuration, not data backups.
    async fn snapshot(&self, name: &str) -> anyhow::Result<CommittedConfiguration>;
    /// Read a named configuration snapshot without changing live desired state.
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
