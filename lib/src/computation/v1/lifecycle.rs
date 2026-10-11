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
    /// A successful stop supersedes an earlier stop failure, because the
    /// component is now stopped. Failures from any other phase stay visible
    /// until the component next starts, so the reason it failed is not lost.
    pub(crate) fn record_stopped(&mut self, lifecycle: ComponentLifecycle) {
        self.lifecycle = lifecycle;
        if self
            .failure
            .as_ref()
            .is_some_and(|failure| failure.phase == FailurePhase::Stop)
        {
            self.failure = None;
        }
        self.transition_time = Utc::now();
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
