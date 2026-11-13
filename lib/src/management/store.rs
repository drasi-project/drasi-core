// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! External configuration-store ownership, acceptance and snapshot contracts.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{AcceptedComponentUpdate, ComponentUpdateCompletion, DesiredInstance};

/// Storage guarantees chosen by the instance creator, never by component data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigurationStoreGuarantees {
    /// Acceptance survives a process restart according to the provider contract.
    pub durable: bool,
    /// Every saved component/resource must have a reconstructible recipe.
    pub requires_reconstructible_definitions: bool,
}

impl ConfigurationStoreGuarantees {
    /// Private in-process storage may retain process-local declarations.
    pub const MEMORY: Self = Self {
        durable: false,
        requires_reconstructible_definitions: false,
    };

    /// Durable storage requires definitions that can be reconstructed after restart.
    pub const DURABLE: Self = Self {
        durable: true,
        requires_reconstructible_definitions: true,
    };

    pub(crate) fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.durable || self.requires_reconstructible_definitions,
            "durable configuration storage requires reconstructible definitions"
        );
        Ok(())
    }
}

/// A durably accepted desired definition, not a checkpoint of running objects.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedConfiguration {
    /// Revision of the durably accepted desired configuration.
    pub revision: u64,
    /// Desired definitions, not the state of running components.
    pub desired: DesiredInstance,
    /// Accepted operations not yet durably confirmed complete. Restoration must
    /// retain these exact descriptions, plans and identities.
    pub pending_updates: Vec<AcceptedComponentUpdate>,
}

/// Idempotent acceptance of one desired-configuration request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceReceipt {
    /// Request identity used to resolve retries.
    pub request_id: String,
    /// Accepted desired-configuration revision.
    pub revision: u64,
    /// Whether acceptance has the store's promised durability.
    pub durable: bool,
    /// Complete assignments and request digest for an accepted authoring operation.
    /// Stored atomically with the definition and receipt, including after deletion.
    pub creation: Option<AcceptedComponentCreation>,
    /// Exact new update assessments accepted by this request, retained after
    /// completion or deletion so a retry never becomes a new operation.
    pub updates: Vec<AcceptedComponentUpdate>,
}

/// Identity assignment retained by the existing idempotency receipt owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedComponentCreation {
    /// All components added by the accepted request.
    pub components: Vec<AcceptedComponentIdentity>,
    /// Ordered allocator transcript, retained even after component deletion.
    pub allocations: Vec<ComponentIdAllocation>,
    /// SHA-256 binding the authoring operation to its complete prepared definition.
    pub request_digest: [u8; 32],
}

/// One component's immutable assignment and creation-time display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedComponentIdentity {
    /// UUID assigned or explicitly accepted for this new component.
    pub id: crate::computation::v1::ComponentId,
    /// The name at acceptance. A later rename does not rewrite old receipts.
    pub name: crate::computation::v1::ComponentName,
}

/// One allocation step. These stored values are data, not reusable admission permits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentIdAllocation {
    /// The caller's optional ID, checked against the immutable instance policy.
    pub requested: Option<crate::computation::v1::ComponentId>,
    /// The generated UUID, or the exact opted-in requested UUID.
    pub assigned: crate::computation::v1::ComponentId,
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
    /// Immutable durability and reconstruction guarantees of this provider.
    fn guarantees(&self) -> ConfigurationStoreGuarantees;

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

    /// Atomically check the revision, store desired state, the optional creation
    /// assignment and its idempotency receipt, and wait for durable completion.
    /// Preserve `creation` exactly in the receipt, including after component
    /// deletion. Identical request IDs, definitions and assignments return the
    /// original receipt even if expected_revision is old.
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
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> anyhow::Result<AcceptanceReceipt>;

    /// Atomically acknowledge exact accepted executions before graph processing
    /// resumes. Do not advance the desired revision or change definitions,
    /// receipts or snapshots. Check the revision and reject a different pending
    /// operation for the same component. Repeating an already completed operation
    /// is harmless. Return the confirmed record; an uncertain result must never
    /// be interpreted by Core as permission to resume.
    async fn complete_updates(
        &self,
        expected_revision: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> anyhow::Result<CommittedConfiguration>;

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
