// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;

use crate::{ChangeEnvelope, OutputReadError, QueryRecoveryIdentity, QueryResultSnapshot};

/// Committed identity and position, without transferring result rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRecoveryHead {
    pub identity: QueryRecoveryIdentity,
    pub sequence: u64,
    pub generation: u64,
}

/// One coherent recovery observation of canonical rows and retained history.
#[derive(Clone)]
pub struct QueryRecoveryView {
    pub identity: QueryRecoveryIdentity,
    pub snapshot: QueryResultSnapshot,
    pub oldest_sequence: Option<u64>,
    pub retained: Option<Vec<ChangeEnvelope>>,
}

impl QueryRecoveryView {
    pub fn head(&self) -> QueryRecoveryHead {
        QueryRecoveryHead {
            identity: self.identity.clone(),
            sequence: self.snapshot.as_of_sequence,
            generation: self.snapshot.generation,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueryHistoryError {
    #[error("query history after {requested} is unavailable; oldest retained sequence {oldest}, latest {latest}")]
    Unavailable {
        requested: u64,
        oldest: u64,
        latest: u64,
    },
    #[error("query output state is poisoned")]
    Poisoned,
    #[error("query output is not ready for recovery")]
    NotReady,
    #[error("query output has no verified producer recovery identity")]
    MissingIdentity,
}

impl From<QueryHistoryError> for OutputReadError {
    fn from(error: QueryHistoryError) -> Self {
        match error {
            QueryHistoryError::NotReady => Self::NotReady(error.to_string()),
            QueryHistoryError::Unavailable {
                requested,
                oldest,
                latest,
            } => Self::Gap {
                requested,
                oldest: Some(oldest),
                latest,
            },
            error => Self::Failed(error.into()),
        }
    }
}

/// Read-only recovery access. It does not own an evaluator or consumer cursor.
#[async_trait]
pub trait QueryRecoveryReader: Send + Sync {
    async fn recovery_head(&self) -> Result<QueryRecoveryHead, OutputReadError>;
    async fn recovery_view(&self, after: Option<u64>)
        -> Result<QueryRecoveryView, OutputReadError>;
    /// Wait for a ready observation different from `after`, subscribing before
    /// checking the head so a concurrent publication cannot be missed.
    async fn wait_recovery(
        &self,
        after: Option<&QueryRecoveryHead>,
    ) -> Result<QueryRecoveryHead, OutputReadError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryRecoveryPolicy {
    #[default]
    Strict,
    AutoReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryPublicationMode {
    #[default]
    Atomic,
    /// Checkpoint/core commit precedes publication; a pending marker fences
    /// uncertain output until reset/recovery.
    NonAtomic,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QueryOptions {
    pub recovery: QueryRecoveryPolicy,
    pub publication: QueryPublicationMode,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryRecoveryError {
    #[error("durable query output belongs to a different producer; automatic reset is forbidden")]
    OwnershipMismatch,
    #[error("source subscription requires reset/rebootstrap before this query can resume")]
    SourceResetRequired,
    #[error("query configuration changed; explicit reset/rebootstrap is required")]
    ConfigurationChanged,
    #[error("query bootstrap did not complete; explicit reset/rebootstrap is required")]
    IncompleteBootstrap,
    #[error("non-atomic query output publication is incomplete")]
    PendingPublication,
    #[error("query reset did not complete")]
    IncompleteReset,
    #[error("inconsistent durable query output: {0}")]
    Inconsistent(String),
}
