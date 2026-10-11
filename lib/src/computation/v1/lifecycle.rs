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

use std::{collections::BTreeMap, sync::Arc};

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
        match self.phase {
            FailurePhase::Activation | FailurePhase::Processing | FailurePhase::Stop => true,
            FailurePhase::Creation => self.disposition == FailureDisposition::Retryable,
            _ => false,
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
    pub exhausted: bool,
    /// A successful start has completed in this construction generation.
    pub started: bool,
    /// Distinguishes initial creation from an explicitly requested lifecycle.
    pub lifecycle_requested: bool,
    pub transition_time: DateTime<Utc>,
}

impl ObservedComponent {
    /// Records that the component stopped successfully.
    ///
    /// `Stopped` is an explicit stop, which acknowledges any failure that
    /// [`ComponentFailure::cleared_by_stop`]; the component events keep its
    /// details. `Failed` records a stop propagated from a failed dependency
    /// and supersedes only an earlier stop failure. Other failures stay
    /// visible until the component next starts.
    pub(crate) fn record_stopped(&mut self, lifecycle: ComponentLifecycle) {
        self.lifecycle = lifecycle;
        if self.failure.as_ref().is_some_and(|failure| {
            failure.phase == FailurePhase::Stop
                || (lifecycle == ComponentLifecycle::Stopped && failure.cleared_by_stop())
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
