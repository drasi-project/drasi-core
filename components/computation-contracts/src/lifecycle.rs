// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Lifecycle and management vocabulary shared by DrasiLib's internal component
//! trait and the native plugin SDK. Each side defines its own hook contexts
//! around the common [`OperationScope`].

use std::{collections::BTreeSet, fmt, sync::Arc};

use serde::{Deserialize, Serialize};
pub use tokio_util::sync::CancellationToken;

use crate::{ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan};

/// How Stop settles the current activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopMode {
    /// Complete in-flight work, then join workers. Used for shutdown and
    /// Stop-boundary updates.
    Drain,
    /// Cancel promptly, then join workers. Used for failure recovery and forced
    /// retirement.
    Abort,
}

/// Why an object is being retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisposeReason {
    /// Host or graph shutdown; state is always preserved.
    Shutdown,
    /// Permanent removal of the component.
    Removal,
    /// A new object replaces this one under an accepted update.
    Replacement,
    /// Initialization failed or was interrupted; state is always preserved.
    InitializationFailure,
}

/// Whether Dispose keeps or erases state owned by this component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StateIntent {
    /// Keep owned state for a later object.
    Preserve,
    /// Erase state owned by this component. Never authorizes erasing borrowed,
    /// shared or another generation's state.
    Erase,
}

impl DisposeReason {
    /// Rejects reason and intent combinations that the contract forbids.
    pub fn validate(self, intent: StateIntent) -> Result<(), LifecycleError> {
        match (self, intent) {
            (Self::Shutdown | Self::InitializationFailure, StateIntent::Erase) => {
                Err(LifecycleError::terminal(anyhow::anyhow!(
                    "dispose reason {self:?} must preserve state"
                )))
            }
            _ => Ok(()),
        }
    }
}

/// Host handling of a failed lifecycle or management operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleErrorKind {
    /// The owner is retained and the operation may be retried.
    Retryable,
    /// The operation failed and is not retried automatically.
    Terminal,
    /// The component does not support the operation; never treated as success.
    Unsupported,
    /// The operation was cancelled; the owner is retained.
    Cancelled,
}

/// A classified lifecycle failure that keeps its source chain.
///
/// Any `anyhow::Error` converts to [`LifecycleErrorKind::Terminal`], so `?`
/// keeps the current error behaviour of ported hook bodies.
pub struct LifecycleError {
    kind: LifecycleErrorKind,
    source: anyhow::Error,
}

/// Result of a lifecycle hook.
pub type LifecycleResult<T = ()> = Result<T, LifecycleError>;

impl LifecycleError {
    /// A failure with an explicit kind.
    pub fn new(kind: LifecycleErrorKind, source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind,
            source: source.into(),
        }
    }

    /// A failure the host may retry on the same owner.
    pub fn retryable(source: impl Into<anyhow::Error>) -> Self {
        Self::new(LifecycleErrorKind::Retryable, source)
    }

    /// A failure that is not retried automatically.
    pub fn terminal(source: impl Into<anyhow::Error>) -> Self {
        Self::new(LifecycleErrorKind::Terminal, source)
    }

    /// The component does not support this operation.
    pub fn unsupported(reason: impl fmt::Display) -> Self {
        Self::new(
            LifecycleErrorKind::Unsupported,
            anyhow::anyhow!("unsupported: {reason}"),
        )
    }

    /// The operation observed cancellation before completing.
    pub fn cancelled() -> Self {
        Self::new(
            LifecycleErrorKind::Cancelled,
            anyhow::anyhow!("lifecycle operation was cancelled"),
        )
    }

    pub fn kind(&self) -> LifecycleErrorKind {
        self.kind
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            LifecycleErrorKind::Retryable | LifecycleErrorKind::Cancelled
        )
    }

    /// The underlying error chain.
    pub fn source_error(&self) -> &anyhow::Error {
        &self.source
    }

    pub fn into_source(self) -> anyhow::Error {
        self.source
    }
}

impl From<anyhow::Error> for LifecycleError {
    fn from(source: anyhow::Error) -> Self {
        let kind = lifecycle_error_kind(&source).unwrap_or(LifecycleErrorKind::Terminal);
        Self::new(kind, source)
    }
}

impl fmt::Debug for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LifecycleError")
            .field("kind", &self.kind)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.source, f)
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Classifies an error chain, treating an unclassified error as terminal.
pub fn lifecycle_error_kind(error: &anyhow::Error) -> Option<LifecycleErrorKind> {
    error
        .chain()
        .take(64)
        .find_map(|cause| cause.downcast_ref::<LifecycleError>())
        .map(LifecycleError::kind)
}

/// A hook that has no work for this component.
pub fn nothing_to_do() -> LifecycleResult {
    Ok(())
}

/// A hook the component does not support. Never a silent no-op.
pub fn unsupported(reason: impl fmt::Display) -> LifecycleResult {
    Err(LifecycleError::unsupported(reason))
}

/// Identity, deadline and cancellation shared by every hook context.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OperationScope {
    operation_id: Arc<str>,
    component_id: Arc<str>,
    generation: u64,
    deadline: Option<tokio::time::Instant>,
    cancellation: CancellationToken,
}

impl OperationScope {
    pub fn new(
        operation_id: impl Into<Arc<str>>,
        component_id: impl Into<Arc<str>>,
        generation: u64,
        deadline: Option<tokio::time::Instant>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            component_id: component_id.into(),
            generation,
            deadline,
            cancellation,
        }
    }

    /// Stable for retries of the same operation.
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    /// The host's construction generation for this object.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// When the host stops waiting. Expiry is neither completion nor rollback.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    /// Time left before the deadline, if any.
    pub fn remaining(&self) -> Option<std::time::Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
    }

    /// Signalled before the host stops waiting for or drops the hook future.
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

/// The boundary a retained or replacement update needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateBoundary {
    /// Apply to the same object inside a completed Pause.
    Pause,
    /// Apply to the same object after Stop, then restart as required.
    Stop,
    /// Dispose the old object and construct a new one.
    Replace,
}

impl From<ComponentUpdateMode> for UpdateBoundary {
    fn from(mode: ComponentUpdateMode) -> Self {
        match mode {
            ComponentUpdateMode::InPlace => Self::Pause,
            ComponentUpdateMode::Restart => Self::Stop,
            ComponentUpdateMode::Recreate => Self::Replace,
        }
    }
}

impl From<UpdateBoundary> for ComponentUpdateMode {
    fn from(boundary: UpdateBoundary) -> Self {
        match boundary {
            UpdateBoundary::Pause => Self::InPlace,
            UpdateBoundary::Stop => Self::Restart,
            UpdateBoundary::Replace => Self::Recreate,
        }
    }
}

/// A side-effect-free answer to a proposed configuration change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "outcome", deny_unknown_fields)]
pub enum Assessment {
    /// The change cannot be applied by this component.
    Unsupported { reason: String },
    /// The change is supported at `boundary` with the given data policy.
    Supported {
        boundary: UpdateBoundary,
        data: ComponentUpdateData,
        /// Names the live facts the assessment relied on; re-checked at execution.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
}

impl Assessment {
    pub fn supported(boundary: UpdateBoundary, data: ComponentUpdateData) -> Self {
        Self::Supported {
            boundary,
            data,
            fingerprint: None,
        }
    }

    pub fn unsupported(reason: impl fmt::Display) -> Self {
        Self::Unsupported {
            reason: reason.to_string(),
        }
    }

    /// The equivalent stored update plan, when supported.
    pub fn plan(&self) -> Option<ComponentUpdatePlan> {
        match self {
            Self::Supported { boundary, data, .. } => Some(ComponentUpdatePlan {
                mode: (*boundary).into(),
                data: *data,
            }),
            Self::Unsupported { .. } => None,
        }
    }

    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Supported { fingerprint, .. } => fingerprint.as_deref(),
            Self::Unsupported { .. } => None,
        }
    }
}

impl From<ComponentUpdatePlan> for Assessment {
    fn from(plan: ComponentUpdatePlan) -> Self {
        Self::supported(plan.mode.into(), plan.data)
    }
}

/// Outcome of a failed management `apply`.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Nothing was mutated; the change now needs a different boundary, if any.
    #[error("update refused: {reason}")]
    Refused {
        now_requires: Option<UpdateBoundary>,
        reason: String,
    },
    /// The object is unchanged.
    #[error("update failed: {0}")]
    Failed(#[source] anyhow::Error),
    /// The object cannot continue and must be stopped and disposed.
    #[error("update damaged the component: {0}")]
    Damaged(#[source] anyhow::Error),
}

impl From<anyhow::Error> for ApplyError {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl ApplyError {
    pub fn damages_object(&self) -> bool {
        matches!(self, Self::Damaged(_))
    }
}

/// Declared once in metadata and checked against actual exports at load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleCapabilities {
    /// Supported update boundaries; a subset of Pause, Stop and Replace.
    #[serde(default)]
    pub update_boundaries: BTreeSet<UpdateBoundary>,
    /// The component sends `Ready` after Start, Resume and restart.
    #[serde(default)]
    pub signals_readiness: bool,
    /// Start receives resolved activation credentials.
    #[serde(default)]
    pub start_credentials: bool,
    /// Instance management (assess and apply) is exported.
    #[serde(default)]
    pub management: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_map_to_stored_modes() {
        for boundary in [
            UpdateBoundary::Pause,
            UpdateBoundary::Stop,
            UpdateBoundary::Replace,
        ] {
            assert_eq!(
                UpdateBoundary::from(ComponentUpdateMode::from(boundary)),
                boundary
            );
        }
    }

    #[test]
    fn anyhow_errors_are_terminal_and_kinds_survive_wrapping() {
        let error = LifecycleError::from(anyhow::anyhow!("boom"));
        assert_eq!(error.kind(), LifecycleErrorKind::Terminal);
        let wrapped = anyhow::Error::new(LifecycleError::retryable(anyhow::anyhow!("later")))
            .context("start");
        assert_eq!(
            lifecycle_error_kind(&wrapped),
            Some(LifecycleErrorKind::Retryable)
        );
        assert_eq!(
            LifecycleError::from(wrapped).kind(),
            LifecycleErrorKind::Retryable
        );
    }

    #[test]
    fn shutdown_and_initialization_failure_cannot_erase() {
        assert!(DisposeReason::Shutdown
            .validate(StateIntent::Erase)
            .is_err());
        assert!(DisposeReason::InitializationFailure
            .validate(StateIntent::Erase)
            .is_err());
        assert!(DisposeReason::Removal.validate(StateIntent::Erase).is_ok());
        assert!(DisposeReason::Replacement
            .validate(StateIntent::Preserve)
            .is_ok());
    }

    #[test]
    fn assessments_roundtrip() -> anyhow::Result<()> {
        let supported = Assessment::Supported {
            boundary: UpdateBoundary::Pause,
            data: ComponentUpdateData::Preserve,
            fingerprint: Some("schema-7".into()),
        };
        let wire = serde_json::to_value(&supported)?;
        assert_eq!(
            wire,
            serde_json::json!({"outcome": "supported", "boundary": "pause", "data": "preserve", "fingerprint": "schema-7"})
        );
        assert_eq!(serde_json::from_value::<Assessment>(wire)?, supported);
        assert_eq!(
            supported.plan(),
            Some(ComponentUpdatePlan {
                mode: ComponentUpdateMode::InPlace,
                data: ComponentUpdateData::Preserve
            })
        );
        assert_eq!(Assessment::unsupported("no").plan(), None);
        Ok(())
    }
}
