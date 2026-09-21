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

use std::{collections::BTreeSet, num::NonZeroUsize, sync::Arc};

use super::{
    data::{validate_identifier, validate_schema},
    ComponentId, ComponentSemanticKind, ContractError, PluginIdentity, PortId, Result,
    SchemaDescriptor, SinkCompletion,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PortDirection {
    Input,
    Output,
}

/// Provider promises which must be negotiated before a graph starts.
/// These descriptors implement no delivery guarantees by themselves.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum PipeCapability {
    /// FIFO for each logical producer stream using authoritative sequence.
    FifoPerStream,
    /// One shared inbox orders admitted events by event time, query-local source
    /// rank, then authoritative source sequence; this is not producer FIFO.
    RankedEventOrder,
    /// Capacity pressure waits/rejects explicitly rather than silently dropping.
    Backpressure,
    /// Successful enqueue survives the provider's documented crash boundary.
    DurableAcceptance,
    /// Delivered events have an explicit, separate handling acknowledgement.
    ExplicitAcknowledgement,
    /// Provider can replay retained data from a supported position.
    Replay,
    /// Retained history within the declared storage lifetime; not crash durability.
    RetainedHistory,
    /// Provider supports atomic delivery/ack operations in a negotiated scope.
    Transactions,
    /// Exactly-once within the transport's negotiated scope, NOT arbitrary
    /// downstream effects. A future host must negotiate that scope separately.
    ExactlyOnce,
}

/// Immutable required guarantees; unsupported requirements are errors.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PipeRequirements {
    required: BTreeSet<PipeCapability>,
}

impl PipeRequirements {
    pub fn new(required: impl IntoIterator<Item = PipeCapability>) -> Self {
        Self {
            required: required.into_iter().collect(),
        }
    }

    pub fn required(&self) -> &BTreeSet<PipeCapability> {
        &self.required
    }
}

/// Immutable capability declaration, not a runtime or persistence configuration.
/// Providers are responsible for actually honoring every advertised guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeCapabilities {
    supported: BTreeSet<PipeCapability>,
    capacity: Option<NonZeroUsize>,
}

impl PipeCapabilities {
    /// Reject inconsistent claims. This validates the declaration's structure,
    /// not the correctness of a pipe implementation or its failure model.
    pub fn try_new(
        supported: impl IntoIterator<Item = PipeCapability>,
        capacity: Option<NonZeroUsize>,
    ) -> Result<Self> {
        use PipeCapability::*;
        let supported: BTreeSet<_> = supported.into_iter().collect();
        for (capability, requires) in [
            (Replay, DurableAcceptance),
            (Transactions, ExplicitAcknowledgement),
            (ExactlyOnce, DurableAcceptance),
            (ExactlyOnce, ExplicitAcknowledgement),
            (ExactlyOnce, Transactions),
        ] {
            if supported.contains(&capability) && !supported.contains(&requires) {
                return Err(ContractError::InvalidCapabilities {
                    capability,
                    requires,
                });
            }
        }
        Ok(Self {
            supported,
            capacity,
        })
    }

    /// The initial bounded in-process pipe's declaration: only FIFO and
    /// backpressure. No durability, replay, acknowledgements, or transactions.
    /// This constructs a descriptor, not a functioning pipe.
    pub fn volatile_bounded(capacity: NonZeroUsize) -> Self {
        Self {
            supported: BTreeSet::from([
                PipeCapability::FifoPerStream,
                PipeCapability::Backpressure,
            ]),
            capacity: Some(capacity),
        }
    }

    pub fn supported(&self) -> &BTreeSet<PipeCapability> {
        &self.supported
    }

    /// Declared maximum buffered envelopes, or no declared finite bound.
    pub const fn capacity(&self) -> Option<NonZeroUsize> {
        self.capacity
    }

    pub fn validate(&self, requirements: &PipeRequirements) -> Result<()> {
        for capability in requirements.required() {
            if !self.supported.contains(capability) {
                return Err(ContractError::UnsupportedCapability {
                    capability: *capability,
                });
            }
        }
        Ok(())
    }
}

/// Named, single-schema port with required transport guarantees.
/// Descriptors contain no resolved configuration, secrets, or runtime contexts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortDescriptor {
    id: PortId,
    direction: PortDirection,
    schema: SchemaDescriptor,
    requirements: PipeRequirements,
}

impl PortDescriptor {
    pub fn new(
        id: PortId,
        direction: PortDirection,
        schema: SchemaDescriptor,
        requirements: PipeRequirements,
    ) -> Self {
        Self {
            id,
            direction,
            schema,
            requirements,
        }
    }

    pub fn id(&self) -> &PortId {
        &self.id
    }

    pub const fn direction(&self) -> PortDirection {
        self.direction
    }

    pub fn schema(&self) -> &SchemaDescriptor {
        &self.schema
    }

    pub fn requirements(&self) -> &PipeRequirements {
        &self.requirements
    }
}

/// Immutable descriptive component interface, not a runtime properties dump.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComponentDescriptor {
    id: ComponentId,
    ports: Arc<[PortDescriptor]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin_identity: Option<PluginIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_kind: Option<ComponentSemanticKind>,
}

impl<'de> serde::Deserialize<'de> for ComponentDescriptor {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Specification {
            id: ComponentId,
            ports: Vec<PortDescriptor>,
            #[serde(default)]
            plugin_identity: Option<PluginIdentity>,
            #[serde(default)]
            semantic_kind: Option<ComponentSemanticKind>,
        }
        let value = Specification::deserialize(deserializer)?;
        let mut descriptor =
            Self::try_new(value.id, value.ports).map_err(serde::de::Error::custom)?;
        if let Some(identity) = value.plugin_identity {
            descriptor = descriptor
                .with_plugin_identity(identity)
                .map_err(serde::de::Error::custom)?;
        }
        if let Some(kind) = value.semantic_kind {
            descriptor = descriptor.with_semantic_kind(kind);
        }
        Ok(descriptor)
    }
}

impl ComponentDescriptor {
    pub fn try_new(id: ComponentId, ports: Vec<PortDescriptor>) -> Result<Self> {
        let mut ids = BTreeSet::new();
        for port in &ports {
            if !ids.insert(port.id()) {
                return Err(ContractError::DuplicatePort {
                    port: port.id().clone(),
                });
            }
        }
        Ok(Self {
            id,
            ports: ports.into(),
            plugin_identity: None,
            semantic_kind: None,
        })
    }

    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    pub fn ports(&self) -> &[PortDescriptor] {
        &self.ports
    }

    /// Explicit semantic category, or none to use the native execution role.
    /// This metadata does not change ports, execution or lifecycle behavior.
    pub const fn semantic_kind(&self) -> Option<ComponentSemanticKind> {
        self.semantic_kind
    }

    /// Declare a host wrapper's meaning independently of its implementation or
    /// configuration. Unannotated descriptors retain their native role's kind.
    pub fn with_semantic_kind(mut self, kind: ComponentSemanticKind) -> Self {
        self.semantic_kind = Some(kind);
        self
    }

    /// Explicit provenance of a preconstructed instance, not a construction recipe.
    pub fn plugin_identity(&self) -> Option<&PluginIdentity> {
        self.plugin_identity.as_ref()
    }

    /// Record host-supplied provenance without inferring it from a Rust type.
    /// Both identifiers reject empty values, whitespace and control characters.
    pub fn with_plugin_identity(mut self, identity: PluginIdentity) -> Result<Self> {
        validate_identifier("plugin", &identity.id)?;
        validate_identifier("plugin version", &identity.version)?;
        self.plugin_identity = Some(identity);
        Ok(self)
    }
}

/// Pure, pre-start edge negotiation: direction, exact full schema descriptor,
/// FIFO baseline, and both endpoints' required guarantees. No hash-only matching.
/// A future host must also validate the DAG, roles, and producer stream ownership;
/// this function neither starts components nor proves provider behavior.
pub fn validate_connection(
    output: &PortDescriptor,
    input: &PortDescriptor,
    pipe: &PipeCapabilities,
) -> Result<()> {
    for (port, expected) in [(output, PortDirection::Output), (input, PortDirection::Input)] {
        if port.direction != expected {
            return Err(ContractError::WrongPortDirection {
                port: port.id.clone(),
                expected,
                actual: port.direction,
            });
        }
    }
    validate_schema(output.schema(), input.schema())?;
    if !pipe.supported().contains(&PipeCapability::RankedEventOrder) {
        pipe.validate(&PipeRequirements::new([PipeCapability::FifoPerStream]))?;
    }
    pipe.validate(output.requirements())?;
    pipe.validate(input.requirements())
}

/// Reject handling-dependent requirements at an acceptance-only sink. A future
/// host must call this for both endpoints' requirements (and any graph-level
/// requirements) before starting a sink. Pipe capabilities never upgrade a sink.
/// Handled is necessary, not sufficient, for end-to-end transactional guarantees.
pub fn validate_sink_completion(
    completion: SinkCompletion,
    requirements: &PipeRequirements,
) -> Result<()> {
    if completion == SinkCompletion::Accepted {
        for required in [
            PipeCapability::ExplicitAcknowledgement,
            PipeCapability::Transactions,
            PipeCapability::ExactlyOnce,
        ] {
            if requirements.required.contains(&required) {
                return Err(ContractError::InsufficientSinkCompletion {
                    actual: completion,
                    required,
                });
            }
        }
    }
    Ok(())
}
