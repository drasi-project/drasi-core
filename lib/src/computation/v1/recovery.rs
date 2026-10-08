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

use std::{collections::BTreeSet, sync::Arc};

use drasi_core::{
    computation::{ComputationIndexes, ComputationQueryError},
    interface::{FailureMode, FailureSurvival, StorageDurability},
};
use serde::{Deserialize, Serialize};

use super::{ComponentId, EdgeDefinition, GraphRevision, QuerySourceProgress};

/// The lifetime in which an explicitly requested recovery guarantee must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryScope {
    MemoryLifetime,
    Failure(FailureMode),
}

impl RecoveryScope {
    pub(crate) fn survives(self, durability: StorageDurability) -> FailureSurvival {
        match self {
            Self::MemoryLifetime => FailureSurvival::Guaranteed,
            Self::Failure(failure) => durability.survival(failure),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RecoveryGuarantee {
    /// Success returned to an upstream producer protects the accepted input.
    Acceptance,
    /// Accepted input can reach the selected boundary again after interruption.
    Replay,
    /// Participating state, input progress and output commit together.
    CommittedProcessing,
    /// Committed output reaches recoverable storage before its owner releases it.
    Publication,
    /// A destination-specific contract prevents duplicate committed effects.
    ExternalEffects,
}

/// An opt-in assertion about every input contributing to one consumer boundary.
/// Unrelated downstream branches are not silently made required subscribers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRequirement {
    pub consumer: ComponentId,
    pub scope: RecoveryScope,
    pub guarantees: BTreeSet<RecoveryGuarantee>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProcessingRecovery {
    #[default]
    Unknown,
    Stateless,
    StatelessWithWakeup,
    Atomic(StorageDurability),
}

/// Immutable instance contract, captured with the component's configuration.
///
/// This is not serializable proof and must not be reconstructed from a factory
/// name or configuration path. Providers remain responsible for their declared
/// storage boundary; a component must actually use the resources it identifies.
/// No contract is inferred for an opaque native or legacy plugin.
#[derive(Debug, Clone, Default)]
pub struct ComponentRecovery {
    pub(crate) admission: Option<StorageDurability>,
    pub(crate) replay_until: Option<ComponentId>,
    pub(crate) processing: ProcessingRecovery,
    pub(crate) publication: Option<StorageDurability>,
    pub(crate) committed_progress: Option<Arc<QuerySourceProgress>>,
    pub(crate) replay_progress: Option<Arc<QuerySourceProgress>>,
    pub(crate) source_admission: Option<Arc<super::SourceAdmission>>,
    pub(crate) output_bindings: Option<Arc<super::output_bindings::OutputBindingState>>,
    pub(crate) delivery: Option<Arc<super::DeliveryRetirementState>>,
}

impl PartialEq for ComponentRecovery {
    fn eq(&self, other: &Self) -> bool {
        let same =
            |a: &Option<Arc<QuerySourceProgress>>, b: &Option<Arc<QuerySourceProgress>>| match (
                a, b,
            ) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
        self.admission == other.admission
            && self.replay_until == other.replay_until
            && self.processing == other.processing
            && self.publication == other.publication
            && same(&self.committed_progress, &other.committed_progress)
            && same(&self.replay_progress, &other.replay_progress)
            && match (&self.output_bindings, &other.output_bindings) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
            && match (&self.source_admission, &other.source_admission) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
            && match (&self.delivery, &other.delivery) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for ComponentRecovery {}

impl ComponentRecovery {
    /// The consumer closes its actual delivery owner through this service.
    pub fn with_delivery_retirement(
        mut self,
        delivery: Arc<super::DeliveryRetirementState>,
    ) -> Self {
        self.delivery = Some(delivery);
        self
    }

    /// Upstream retention declaration, without transaction-owner proof.
    /// Boundary adapters must independently bind the actual progress resource.
    pub fn replay_retention(&self) -> Option<(StorageDurability, &ComponentId)> {
        self.admission.zip(self.replay_until.as_ref())
    }

    pub(crate) fn with_output_bindings(
        mut self,
        bindings: Arc<super::output_bindings::OutputBindingState>,
    ) -> Self {
        self.output_bindings = Some(bindings);
        self
    }

    /// A deterministic, stateless transformation preserving replay identity.
    /// It must not own hidden state, timers, workers, or external effects.
    pub fn stateless() -> Self {
        Self {
            processing: ProcessingRecovery::Stateless,
            ..Self::default()
        }
    }

    /// The component uses this actual transaction for state, progress and output.
    /// Independent or incomplete writers cannot produce this contract.
    pub fn transactional(indexes: &ComputationIndexes) -> Result<Self, ComputationQueryError> {
        indexes.atomic_result_transaction()?;
        Ok(Self {
            processing: ProcessingRecovery::Atomic(indexes.durability()),
            publication: Some(indexes.durability()),
            ..Self::default()
        })
    }

    /// Per-operation consumer state and completion use this actual transaction.
    /// A sink publishes no outbox and this is not an external-effect guarantee.
    pub fn transactional_consumer(
        indexes: &ComputationIndexes,
    ) -> Result<Self, ComputationQueryError> {
        let mut contract = Self::transactional(indexes)?;
        contract.publication = None;
        Ok(contract)
    }

    /// The progress publication owned by this component's validated transaction.
    /// A replaying source must hold this same resource, not a matching name.
    pub fn with_committed_progress(mut self, progress: Arc<QuerySourceProgress>) -> Self {
        self.committed_progress = Some(progress);
        self
    }

    /// An ingress implementation acknowledges only its declared storage commit.
    /// Without `replay_until`, this protects admission, not downstream delivery.
    pub fn admitted(durability: StorageDurability) -> Self {
        Self {
            admission: Some(durability),
            ..Self::default()
        }
    }

    /// The source retains input until this consumer commits its progress, not
    /// until an intermediate queue accepts it. The consumer must be on the path
    /// and provide atomic progress; this is checked against the constructed graph.
    pub fn replay_until(mut self, consumer: ComponentId) -> Self {
        self.replay_until = Some(consumer);
        self
    }

    /// Retain admitted input until an outgoing durable send succeeds, and replay
    /// it after interruption. A volatile outgoing receipt is not sufficient.
    pub fn replay_to_durable_delivery(mut self) -> Self {
        self.publication = self.admission;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RecoveryParticipant {
    Component(ComponentId),
    Connection(EdgeDefinition),
    Subscription {
        producer: ComponentId,
        consumer: ComponentId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RecoveryIncompatibility {
    UnknownComponent,
    NoDataInput,
    EmptyRequirement,
    UnknownAdmission,
    StorageSurvival { actual: FailureSurvival },
    LossyDelivery,
    UnknownSubscription,
    UnknownTransport,
    MissingReplayBoundary,
    UnknownProcessing,
    UntrackedWakeupState,
    NonAtomicProcessing,
    UnhandledAcceptance,
    MissingPublication,
    MissingExternalEffectContract,
    ReplayConsumerOutsidePath,
    ReplayConsumerWithoutAtomicProgress,
    MismatchedProgressResource,
    ContractChangedDuringActivation,
    AmbiguousReplayPath,
    CyclicPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{participant:?} cannot provide {guarantee:?}: {reason:?}")]
pub struct RecoveryIssue {
    pub participant: RecoveryParticipant,
    pub guarantee: RecoveryGuarantee,
    pub reason: RecoveryIncompatibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryPathReport {
    pub graph_id: Arc<str>,
    pub revision: GraphRevision,
    pub requirement: RecoveryRequirement,
    pub participants: BTreeSet<ComponentId>,
    pub issues: Vec<RecoveryIssue>,
}

impl RecoveryPathReport {
    pub fn satisfied(&self) -> bool {
        self.issues.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("recovery requirement for {consumer} is not satisfied: {issues:?}")]
pub struct RecoveryValidationError {
    pub consumer: ComponentId,
    pub issues: Vec<RecoveryIssue>,
}

impl RecoveryPathReport {
    pub(crate) fn validate(self) -> Result<(), RecoveryValidationError> {
        if self.satisfied() {
            Ok(())
        } else {
            Err(RecoveryValidationError {
                consumer: self.requirement.consumer,
                issues: self.issues,
            })
        }
    }
}
