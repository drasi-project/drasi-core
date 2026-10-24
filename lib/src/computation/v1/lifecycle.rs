// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chrono::{DateTime, Utc};

use super::{ComponentId, EdgeDefinition, GraphError, ResourceId};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct GraphRevision(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComponentGeneration(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OperationEpoch(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealizationState {
    Pending,
    Creating,
    Created,
    Blocked,
    CreationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentLifecycle {
    Stopped,
    Starting,
    Running,
    Quiescing,
    Quiesced,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentHealth {
    Unknown,
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingState {
    Declared,
    Binding,
    Bound,
    Failed,
    Draining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataAvailability {
    Unknown,
    Idle,
    Available,
    Unavailable,
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePhase {
    Validation,
    Creation,
    Binding,
    Activation,
    Processing,
    Stop,
    Removal,
    Control,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    Retryable,
    Terminal,
}

/// Marks an unwinding extension failure as damage to its construction generation.
///
/// Adapters which catch a panic outside Rust (including a native plugin ABI)
/// should return this cause. Successful cleanup does not authorize reuse of the
/// object; replacement requires a new, independently constructed generation.
#[derive(Debug, thiserror::Error)]
#[error("component panicked; its generation is quarantined: {source}")]
pub struct ComponentPanic {
    #[source]
    pub source: anyhow::Error,
}

impl ComponentPanic {
    pub fn new(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

/// Bounded diagnostics for an unwind caught by the graph-owned task boundary.
#[derive(Debug, thiserror::Error)]
#[error("component {component} generation {generation:?} operation {operation:?} panicked during {phase:?}: {message}")]
pub struct ComponentUnwind {
    pub component: ComponentId,
    pub generation: ComponentGeneration,
    pub operation: OperationEpoch,
    pub phase: FailurePhase,
    pub message: String,
}

/// Interrupted mutable work has no rollback proof. Cleanup may release its
/// resources, but only replacement can authorize another construction generation.
#[derive(Debug, thiserror::Error)]
#[error("component {component} generation {generation:?} operation {operation:?} was interrupted during {phase:?} ({reason}); its generation is quarantined")]
pub struct ComponentInterrupted {
    pub component: ComponentId,
    pub generation: ComponentGeneration,
    pub operation: OperationEpoch,
    pub phase: FailurePhase,
    pub reason: &'static str,
}

/// Bounded diagnostics for a graph-owned resource callback's unwind.
#[derive(Debug, thiserror::Error)]
#[error("resource {resource} generation {generation} operation {operation:?} panicked during {phase:?}: {message}")]
pub struct ResourceUnwind {
    pub resource: ResourceId,
    pub generation: u64,
    pub operation: OperationEpoch,
    pub phase: FailurePhase,
    pub message: String,
}

pub(crate) fn bounded_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else {
        "non-string panic payload"
    };
    let mut end = message.len().min(2048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let message = message[..end].to_owned();
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
        // Do not recursively destroy an adversarial payload from a second panic.
        std::mem::forget(payload);
    }
    message
}

/// A classified native operation failure. Retryability never proves rollback or cleanup.
#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct ComponentOperationError {
    pub disposition: FailureDisposition,
    #[source]
    pub source: anyhow::Error,
}

impl ComponentOperationError {
    pub fn new(disposition: FailureDisposition, source: impl Into<anyhow::Error>) -> Self {
        Self {
            disposition,
            source: source.into(),
        }
    }
}

pub(crate) fn operation_failure_disposition(error: &anyhow::Error) -> FailureDisposition {
    if error
        .chain()
        .take(64)
        .any(|cause| cause.is::<ComponentPanic>() || cause.is::<ComponentInterrupted>())
    {
        return FailureDisposition::Terminal;
    }
    for cause in error.chain().take(32) {
        if let Some(error) = cause.downcast_ref::<ComponentOperationError>() {
            return error.disposition;
        }
        if let Some(error) = cause.downcast_ref::<super::ComponentCreationError>() {
            return error.disposition;
        }
        if let Some(error) = cause.downcast_ref::<GraphError>() {
            return error.failure_disposition();
        }
        if let Some(error) = cause.downcast_ref::<super::PipeError>() {
            return pipe_failure_disposition(error);
        }
        if let Some(error) = cause.downcast_ref::<drasi_core::computation::ComputationQueryError>()
        {
            use drasi_core::computation::ComputationQueryError;
            match error {
                ComputationQueryError::Rollback { .. }
                | ComputationQueryError::RecoveryRequired
                | ComputationQueryError::TransactionMismatch
                | ComputationQueryError::AtomicOutputUnsupported => {
                    return FailureDisposition::Terminal
                }
                ComputationQueryError::OperationInProgress => return FailureDisposition::Retryable,
                ComputationQueryError::Index(error) => {
                    if let Some(disposition) = index_failure_disposition(error) {
                        return disposition;
                    }
                }
                ComputationQueryError::Build(_) | ComputationQueryError::Evaluation(_) => {}
            }
        }
        if cause.is::<super::DeliveryError>() {
            // The delivery runner already applied its handling retry budget.
            return FailureDisposition::Terminal;
        }
        if let Some(error) = cause.downcast_ref::<drasi_core::interface::IndexError>() {
            if let Some(disposition) = index_failure_disposition(error) {
                return disposition;
            }
        }
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return match error.kind() {
                std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::AddrInUse => FailureDisposition::Retryable,
                _ => FailureDisposition::Terminal,
            };
        }
    }
    FailureDisposition::Terminal
}

fn index_failure_disposition(
    error: &drasi_core::interface::IndexError,
) -> Option<FailureDisposition> {
    use drasi_core::interface::IndexError;
    match error {
        IndexError::IOError | IndexError::ConnectionFailed(_) => {
            Some(FailureDisposition::Retryable)
        }
        IndexError::Other(_) => None,
        IndexError::NotSupported
        | IndexError::ArchiveNotEnabled
        | IndexError::CorruptedData
        | IndexError::UnknownStore(_) => Some(FailureDisposition::Terminal),
    }
}

impl GraphError {
    pub(crate) fn damages_generation(&self) -> bool {
        use std::error::Error;
        let mut cause: Option<&(dyn Error + 'static)> = Some(self);
        for _ in 0..64 {
            let Some(error) = cause else {
                break;
            };
            if error.is::<ComponentPanic>() || error.is::<ComponentInterrupted>() {
                return true;
            }
            if let Some(GraphError::Cleanup { primary, errors }) = error.downcast_ref() {
                return primary
                    .as_ref()
                    .is_some_and(|error| error.damages_generation())
                    || errors.iter().any(GraphError::damages_generation);
            }
            cause = error.source();
        }
        false
    }

    /// Classifies reported work. A component-returned error never authorizes killing the driver.
    pub(crate) fn failure_disposition(&self) -> FailureDisposition {
        use FailureDisposition::{Retryable, Terminal};
        match self {
            Self::Creation { disposition, .. } => *disposition,
            Self::Component { source, .. }
            | Self::Driver { source, .. }
            | Self::ResourceCreation { source, .. }
            | Self::ResourceCleanup { source, .. } => operation_failure_disposition(source),
            Self::Reported { cause } => cause.failure_disposition(),
            Self::AdditionRejected { cause, .. } => cause.failure_disposition(),
            Self::Pipe { source, .. } => pipe_failure_disposition(source),
            Self::Forward {
                accepted_branches,
                source,
                ..
            } => {
                if *accepted_branches == 0 {
                    pipe_failure_disposition(&source.error)
                } else {
                    Terminal
                }
            }
            Self::Cleanup { primary, errors } => {
                let mut causes = primary
                    .iter()
                    .map(|cause| cause.as_ref())
                    .chain(errors.iter());
                if primary.is_none() && errors.is_empty()
                    || causes.any(|cause| cause.failure_disposition() == Terminal)
                {
                    Terminal
                } else {
                    Retryable
                }
            }
            Self::Control(error) => match error {
                super::ControlError::QueueFull { .. } => Retryable,
                super::ControlError::InvalidCapacity { .. }
                | super::ControlError::PlaneClosed
                | super::ControlError::Closed { .. }
                | super::ControlError::NotNeighbor { .. }
                | super::ControlError::StaleComponent { .. }
                | super::ControlError::StaleConnection { .. }
                | super::ControlError::SelfConnection { .. }
                | super::ControlError::PayloadTooLarge { .. }
                | super::ControlError::PayloadTooDeep { .. }
                | super::ControlError::Poisoned => Terminal,
            },
            Self::StaleRevision { .. }
            | Self::OperationInProgress
            | Self::ObservationQueueFull
            | Self::ReconciliationTimeout
            | Self::StartupIncomplete
            | Self::DependencyUnavailable { .. }
            | Self::CreationStopped { .. }
            | Self::StartSuperseded { .. }
            | Self::StopTimeout { .. }
            | Self::Cancelled => Retryable,
            Self::Contract(_)
            | Self::Recovery(_)
            | Self::Validation { .. }
            | Self::Topology { .. }
            | Self::InvalidState { .. }
            | Self::StaleGeneration
            | Self::ControllerClosed
            | Self::Emission { .. } => Terminal,
        }
    }
}

fn pipe_failure_disposition(error: &super::PipeError) -> FailureDisposition {
    match error {
        super::PipeError::Backend(source) => operation_failure_disposition(source),
        super::PipeError::CapacityExhausted => FailureDisposition::Retryable,
        super::PipeError::InvalidCapacity
        | super::PipeError::Contract(_)
        | super::PipeError::Closed
        | super::PipeError::ReceiverTaken
        | super::PipeError::Lagged { .. }
        | super::PipeError::PositionUnavailable { .. }
        | super::PipeError::AcceptanceUnknown { .. }
        | super::PipeError::AcknowledgementUnknown { .. } => FailureDisposition::Terminal,
    }
}

#[derive(Debug, Clone)]
pub struct ComponentFailure {
    pub phase: FailurePhase,
    pub disposition: FailureDisposition,
    pub cause: Arc<GraphError>,
    pub timestamp: DateTime<Utc>,
}

impl ComponentFailure {
    /// Whether an explicit stop acknowledges this failure.
    ///
    /// Activation, processing, and stop failures belong to one run, and the
    /// next start begins a new one. A retryable creation failure is retried by
    /// the next start. Terminal creation failures and validation, binding,
    /// removal, and control failures need a specification or topology change,
    /// so stopping cannot resolve them.
    pub(crate) fn cleared_by_stop(&self) -> bool {
        if self.cause.damages_generation() {
            return false;
        }
        match self.phase {
            FailurePhase::Activation | FailurePhase::Processing | FailurePhase::Stop => true,
            FailurePhase::Creation => self.disposition == FailureDisposition::Retryable,
            _ => false,
        }
    }
}

/// An unavailable prerequisite, not a processing or configuration failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DependencyWait {
    Component {
        id: ComponentId,
    },
    Resource {
        id: ResourceId,
    },
    Factory {
        implementation: super::ImplementationIdentity,
    },
    Connection {
        endpoint: super::Endpoint,
    },
    Stream {
        endpoint: super::Endpoint,
    },
}

impl std::fmt::Display for DependencyWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Component { id } => write!(f, "component '{id}'"),
            Self::Resource { id } => write!(f, "resource '{id}'"),
            Self::Factory { implementation } => {
                write!(
                    f,
                    "factory '{}' version '{}'",
                    implementation.name, implementation.version
                )
            }
            Self::Connection { endpoint } => write!(
                f,
                "connection for '{}:{}'",
                endpoint.component, endpoint.port
            ),
            Self::Stream { endpoint } => write!(
                f,
                "output stream for '{}:{}'",
                endpoint.component, endpoint.port
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ObservedComponent {
    pub generation: ComponentGeneration,
    pub operation: OperationEpoch,
    pub revision: GraphRevision,
    pub realization: RealizationState,
    pub lifecycle: ComponentLifecycle,
    pub health: ComponentHealth,
    pub failure: Option<ComponentFailure>,
    pub waiting_for: Vec<DependencyWait>,
    pub exhausted: bool,
    /// A successful start has completed in this construction generation.
    pub started: bool,
    /// Distinguishes initial creation from an explicitly requested lifecycle.
    pub lifecycle_requested: bool,
    pub transition_time: DateTime<Utc>,
}

impl ObservedComponent {
    pub fn waiting_reason(&self) -> Option<String> {
        if self.waiting_for.is_empty() {
            None
        } else {
            Some(format!(
                "Waiting for {}",
                self.waiting_for
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }

    /// Records that the component stopped successfully.
    ///
    /// `Stopped` is an explicit stop, which acknowledges any failure that
    /// [`ComponentFailure::cleared_by_stop`]; the component events keep its
    /// details. `Failed` records a stop propagated from a failed dependency
    /// and supersedes only an earlier stop failure. Other failures stay
    /// visible until the component next starts.
    pub(crate) fn record_stopped(&mut self, lifecycle: ComponentLifecycle) {
        let damaged = self
            .failure
            .as_ref()
            .is_some_and(|failure| failure.cause.damages_generation());
        self.lifecycle = if damaged {
            ComponentLifecycle::Failed
        } else {
            lifecycle
        };
        if self.realization == RealizationState::Created {
            self.waiting_for.clear();
        }
        if self.failure.as_ref().is_some_and(|failure| {
            !damaged
                && (failure.phase == FailurePhase::Stop
                    || (lifecycle == ComponentLifecycle::Stopped && failure.cleared_by_stop()))
        }) {
            self.failure = None;
        }
        self.transition_time = Utc::now();
    }

    /// Acknowledges the failure of a component that has nothing left to stop.
    ///
    /// An explicit stop of a component whose creation, start, or processing
    /// failed finds it already inactive, but must still leave it stopped. A
    /// creation failure keeps `RealizationState::CreationFailed`, so the next
    /// start retries creation.
    /// Returns whether a failure was acknowledged.
    pub(crate) fn acknowledge_stopped(&mut self) -> bool {
        if !self
            .failure
            .as_ref()
            .is_some_and(ComponentFailure::cleared_by_stop)
        {
            return false;
        }
        self.record_stopped(ComponentLifecycle::Stopped);
        true
    }
}

#[derive(Debug, Clone)]
pub struct ObservedRelationship {
    pub binding: BindingState,
    pub availability: DataAvailability,
    pub generation: u64,
    pub revision: GraphRevision,
    pub failure: Option<ComponentFailure>,
    pub transition_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationSummary {
    Completed,
    CompletedWithFailures,
    Rejected,
}

#[derive(Debug, Clone)]
pub enum CreationOutcome {
    Created,
    CreationFailed(ComponentFailure),
    Blocked {
        dependencies: Vec<ComponentId>,
        resources: Vec<ResourceId>,
    },
    NotAttempted,
}

#[derive(Debug, Clone)]
pub struct DeploymentReport {
    pub revision: GraphRevision,
    pub summary: OperationSummary,
    pub components: BTreeMap<ComponentId, CreationOutcome>,
    pub resources: BTreeMap<ResourceId, ResourceRealization>,
}

#[derive(Debug, Clone)]
pub enum StartOutcome {
    Started,
    StartFailed(ComponentFailure),
    AlreadyRunning,
    NotCreated,
    NotRequested,
    Blocked { dependencies: Vec<ComponentId> },
}

#[derive(Debug, Clone)]
pub struct StartReport {
    pub revision: GraphRevision,
    pub summary: OperationSummary,
    pub components: BTreeMap<ComponentId, StartOutcome>,
}

#[derive(Debug, Clone)]
pub enum StopOutcome {
    Stopped,
    AlreadyStopped,
    NotCreated,
    StopFailed(ComponentFailure),
}

#[derive(Debug, Clone)]
pub struct StopReport {
    pub revision: GraphRevision,
    pub summary: OperationSummary,
    pub components: BTreeMap<ComponentId, StopOutcome>,
}

/// A forced interruption is separate from its subsequent cleanup. Completed
/// cleanup is not proof that the interrupted work rolled back.
#[derive(Debug, Clone)]
pub struct ForceCancellationReport {
    pub revision: GraphRevision,
    pub summary: OperationSummary,
    pub interrupted: BTreeMap<ComponentId, ComponentFailure>,
    pub cleanup: StopReport,
}

#[derive(Debug, thiserror::Error)]
#[error("force cancellation requires the complete actual resource domain; requested {requested:?}, required {required:?}")]
pub struct ForceCancellationScopeError {
    pub requested: BTreeSet<ComponentId>,
    pub required: BTreeSet<ComponentId>,
}

#[derive(Debug, thiserror::Error)]
#[error("force cancellation cannot prove resource {resource} is safe while construction or cleanup is incomplete")]
pub struct ForceCancellationResourceUnavailable {
    pub resource: ResourceId,
}

/// A lifecycle request completed with unsuccessful members. The complete report
/// retains every member's outcome; the source chain follows its first failure.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LifecycleReportError {
    Start(StartReport),
    Stop(StopReport),
    Reconciliation(super::ReconciliationReport),
}

impl std::fmt::Display for LifecycleReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(report) => write!(f, "native activation failed: {:?}", report.components),
            Self::Stop(report) => write!(f, "native stop failed: {:?}", report.components),
            Self::Reconciliation(_) => write!(
                f,
                "one or more components failed; inspect graph observations"
            ),
        }
    }
}

impl std::error::Error for LifecycleReportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        let failure = match self {
            Self::Start(report) => report
                .components
                .values()
                .find_map(|outcome| match outcome {
                    StartOutcome::StartFailed(failure) => Some(failure),
                    _ => None,
                }),
            Self::Stop(report) => report
                .components
                .values()
                .find_map(|outcome| match outcome {
                    StopOutcome::StopFailed(failure) => Some(failure),
                    _ => None,
                }),
            Self::Reconciliation(report) => report.failures.first(),
        };
        failure.map(|failure| failure.cause.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[derive(Debug, Clone)]
pub struct ObservedGraph {
    pub revision: GraphRevision,
    pub run_epoch: u64,
    pub components: BTreeMap<ComponentId, ObservedComponent>,
    pub relationships: BTreeMap<EdgeDefinition, ObservedRelationship>,
    pub resources: BTreeMap<ResourceId, ObservedResource>,
    pub deployment: Option<DeploymentReport>,
    pub startup: Option<StartReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRealization {
    Pending,
    Created,
    CleanupRequired,
    Released,
}

#[derive(Debug, Clone)]
pub struct ObservedResource {
    pub realization: ResourceRealization,
    pub revision: GraphRevision,
    pub generation: u64,
    pub transition_time: DateTime<Utc>,
    pub failure: Option<ComponentFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ActivationCoupling {
    Independent,
    RequiresRunning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalPolicy {
    Reject,
    Cascade,
    Orphan,
    Drain,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelationshipPolicy {
    pub required_for_creation: bool,
    pub required_for_binding: bool,
    pub dynamically_replaceable: bool,
    #[serde(default)]
    pub rebind_on_consumer_replace: bool,
    pub activation: ActivationCoupling,
    pub propagate_failure: bool,
    /// Stop the graph-owned producer if this consumer fails, for example to
    /// release an isolated subscription to a shared, borrowed legacy Source.
    #[serde(default)]
    pub fence_producer_on_failure: bool,
    pub orphan_permitted: bool,
}

impl Default for RelationshipPolicy {
    fn default() -> Self {
        Self {
            required_for_creation: false,
            required_for_binding: true,
            dynamically_replaceable: true,
            rebind_on_consumer_replace: false,
            activation: ActivationCoupling::Independent,
            propagate_failure: false,
            fence_producer_on_failure: false,
            orphan_permitted: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LifecyclePolicy {
    pub auto_start: bool,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self { auto_start: true }
    }
}

#[derive(Debug, Clone)]
pub struct HealthObservation {
    pub component: ComponentId,
    pub generation: ComponentGeneration,
    pub operation: OperationEpoch,
    pub health: ComponentHealth,
}

#[derive(Debug, Clone)]
pub enum GraphSelection {
    Exact(Vec<ComponentId>),
    Dependencies(Vec<ComponentId>),
    Dependents(Vec<ComponentId>),
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_policy_preserves_classification_through_context_and_reporting() {
        for disposition in [FailureDisposition::Retryable, FailureDisposition::Terminal] {
            let source = anyhow::Error::new(ComponentOperationError::new(
                disposition,
                drasi_core::interface::IndexError::IOError,
            ))
            .context("native operation");
            let error = GraphError::Component {
                component: ComponentId::try_new("query").unwrap(),
                operation: "process",
                source,
            };
            assert_eq!(error.failure_disposition(), disposition);
            let reported = GraphError::Reported {
                cause: Arc::new(error),
            };
            assert_eq!(reported.failure_disposition(), disposition);
            assert_eq!(failure_from_report(reported).disposition, disposition);
        }
    }

    fn failure_from_report(error: GraphError) -> ComponentFailure {
        ComponentFailure {
            disposition: error.failure_disposition(),
            phase: FailurePhase::Processing,
            cause: Arc::new(error),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn io_failures_and_corruption_do_not_share_a_retry_policy() {
        use drasi_core::{
            computation::ComputationQueryError, evaluation::EvaluationError, interface::IndexError,
        };
        for evaluation in [false, true] {
            for (cause, expected) in [
                (IndexError::IOError, FailureDisposition::Retryable),
                (IndexError::CorruptedData, FailureDisposition::Terminal),
                (IndexError::NotSupported, FailureDisposition::Terminal),
            ] {
                let cause = if evaluation {
                    ComputationQueryError::Evaluation(EvaluationError::IndexError(cause))
                } else {
                    ComputationQueryError::Index(cause)
                };
                let cause = anyhow::Error::new(cause)
                    .context("evaluating input")
                    .context("native query");
                assert_eq!(operation_failure_disposition(&cause), expected);
            }
        }
        assert_eq!(
            operation_failure_disposition(&anyhow::anyhow!("retryable transient error")),
            FailureDisposition::Terminal,
            "messages are not a retry protocol"
        );
    }

    #[test]
    fn structural_failure_policy_precedes_nested_transient_causes() {
        use drasi_core::{computation::ComputationQueryError, interface::IndexError};
        let errors = [
            anyhow::Error::new(ComputationQueryError::Rollback {
                failure: Box::new(ComputationQueryError::Index(IndexError::IOError)),
                rollback: IndexError::IOError,
            }),
            anyhow::Error::new(super::super::PipeError::AcceptanceUnknown {
                source: IndexError::IOError.into(),
            }),
            anyhow::Error::new(GraphError::Cleanup {
                primary: Some(Box::new(GraphError::Component {
                    component: ComponentId::try_new("query").unwrap(),
                    operation: "process",
                    source: IndexError::IOError.into(),
                })),
                errors: vec![GraphError::Topology {
                    reason: "corrupt ownership".into(),
                }],
            }),
        ];
        for error in errors {
            let error = error
                .context("native operation")
                .context("reported failure");
            assert_eq!(
                operation_failure_disposition(&error),
                FailureDisposition::Terminal
            );
        }
    }

    #[test]
    fn uncertain_pipe_effects_and_mixed_cleanup_failures_are_terminal() {
        for pipe in [
            super::super::PipeError::AcceptanceUnknown {
                source: drasi_core::interface::IndexError::IOError.into(),
            },
            super::super::PipeError::AcknowledgementUnknown {
                source: drasi_core::interface::IndexError::IOError.into(),
            },
            super::super::PipeError::PositionUnavailable {
                requested: 4,
                oldest: 8,
            },
        ] {
            assert_eq!(
                pipe_failure_disposition(&pipe),
                FailureDisposition::Terminal
            );
        }
        let transient = || GraphError::StopTimeout {
            component: ComponentId::try_new("query").unwrap(),
        };
        assert_eq!(
            GraphError::Cleanup {
                primary: Some(Box::new(transient())),
                errors: vec![transient()],
            }
            .failure_disposition(),
            FailureDisposition::Retryable
        );
        assert_eq!(
            GraphError::Cleanup {
                primary: Some(Box::new(transient())),
                errors: vec![GraphError::Topology {
                    reason: "invalid binding".into()
                }],
            }
            .failure_disposition(),
            FailureDisposition::Terminal
        );
    }

    fn failure(phase: FailurePhase, disposition: FailureDisposition) -> ComponentFailure {
        ComponentFailure {
            phase,
            disposition,
            cause: Arc::new(GraphError::Cancelled),
            timestamp: Utc::now(),
        }
    }

    fn observed(
        realization: RealizationState,
        lifecycle: ComponentLifecycle,
        failure: Option<ComponentFailure>,
    ) -> ObservedComponent {
        ObservedComponent {
            generation: ComponentGeneration(1),
            operation: OperationEpoch(1),
            revision: GraphRevision(1),
            realization,
            lifecycle,
            health: ComponentHealth::Unknown,
            failure,
            waiting_for: Vec::new(),
            exhausted: false,
            started: false,
            lifecycle_requested: true,
            transition_time: Utc::now(),
        }
    }

    #[test]
    fn explicit_stop_clears_only_failures_a_restart_can_resolve() {
        use FailureDisposition::{Retryable, Terminal};
        use FailurePhase::*;
        for (phase, disposition, cleared) in [
            (Activation, Terminal, true),
            (Processing, Terminal, true),
            (Stop, Retryable, true),
            (Creation, Retryable, true),
            (Creation, Terminal, false),
            (Validation, Retryable, false),
            (Binding, Retryable, false),
            (Removal, Retryable, false),
            (Control, Retryable, false),
        ] {
            let mut node = observed(
                RealizationState::Created,
                ComponentLifecycle::Failed,
                Some(failure(phase, disposition)),
            );
            node.record_stopped(ComponentLifecycle::Stopped);
            assert_eq!(node.lifecycle, ComponentLifecycle::Stopped);
            assert_eq!(
                node.failure.is_none(),
                cleared,
                "{phase:?}/{disposition:?} after an explicit stop"
            );
        }
    }

    #[test]
    fn propagated_stop_keeps_the_failure_that_explains_it() {
        let mut node = observed(
            RealizationState::Created,
            ComponentLifecycle::Running,
            Some(failure(
                FailurePhase::Processing,
                FailureDisposition::Retryable,
            )),
        );
        node.record_stopped(ComponentLifecycle::Failed);
        assert_eq!(node.lifecycle, ComponentLifecycle::Failed);
        assert!(node.failure.is_some());

        node.failure = Some(failure(FailurePhase::Stop, FailureDisposition::Retryable));
        node.record_stopped(ComponentLifecycle::Failed);
        assert!(
            node.failure.is_none(),
            "a successful stop supersedes a stop failure"
        );
    }

    #[test]
    fn acknowledging_an_inactive_failure_leaves_creation_to_be_retried() {
        let mut node = observed(
            RealizationState::CreationFailed,
            ComponentLifecycle::Stopped,
            Some(failure(
                FailurePhase::Creation,
                FailureDisposition::Retryable,
            )),
        );
        assert!(node.acknowledge_stopped());
        assert!(node.failure.is_none());
        assert_eq!(node.lifecycle, ComponentLifecycle::Stopped);
        assert_eq!(node.realization, RealizationState::CreationFailed);
        assert!(!node.acknowledge_stopped(), "nothing left to acknowledge");

        let mut terminal = observed(
            RealizationState::CreationFailed,
            ComponentLifecycle::Stopped,
            Some(failure(
                FailurePhase::Creation,
                FailureDisposition::Terminal,
            )),
        );
        assert!(!terminal.acknowledge_stopped());
        assert!(terminal.failure.is_some());
    }
}
