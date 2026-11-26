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

/// How a component's outputs relate to their inputs' replay progress.
///
/// This applies to every output of the component and does not by itself
/// establish durability or a valid producer identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputProgress {
    /// Outputs have their own progress boundary.
    Origin,
    /// Outputs carry forward their input's progress boundary.
    Relay,
}

/// Provider promises which must be negotiated before a graph starts.
/// These descriptors implement no delivery guarantees by themselves.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum PipeCapability {
    /// FIFO for each logical producer stream using authoritative sequence.
    FifoPerStream,
    /// Merge available source heads by event time and query-local rank, without
    /// moving a later logical source sequence ahead of an earlier admitted one.
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

/// What a port carries. Ports of different kinds never connect.
///
/// Data outputs fan out live changes to their connected inputs. A connection
/// can also carry an addressed current-state session for its own subscriber.
/// Bootstrap ports describe those sessions and allow a local provider to be
/// attached. Either kind of port may have no connections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PortKind {
    #[default]
    Data,
    Bootstrap,
}

impl PortKind {
    pub const fn is_data(&self) -> bool {
        matches!(self, Self::Data)
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
    #[serde(default, skip_serializing_if = "PortKind::is_data")]
    kind: PortKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state_for: Option<PortId>,
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
            kind: PortKind::Data,
            state_for: None,
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

    /// A bootstrap port with the given snapshot record schema.
    pub fn bootstrap(
        id: PortId,
        direction: PortDirection,
        schema: SchemaDescriptor,
        requirements: PipeRequirements,
    ) -> Self {
        Self::new(id, direction, schema, requirements).with_kind(PortKind::Bootstrap)
    }

    pub fn with_kind(mut self, kind: PortKind) -> Self {
        self.kind = kind;
        self
    }

    /// Associate this bootstrap port with a particular data port. A descriptor
    /// with several inputs or outputs of the same schema must not guess.
    pub fn for_data_port(mut self, data: PortId) -> Self {
        self.state_for = Some(data);
        self
    }

    pub const fn kind(&self) -> PortKind {
        self.kind
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
    #[serde(skip_serializing_if = "Option::is_none")]
    output_progress: Option<OutputProgress>,
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
            #[serde(default)]
            output_progress: Option<OutputProgress>,
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
        if let Some(progress) = value.output_progress {
            descriptor = descriptor.with_output_progress(progress);
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
        let mut assigned = BTreeSet::new();
        for state in &ports {
            if let Some(data) = &state.state_for {
                if state.kind != PortKind::Bootstrap
                    || !assigned.insert(data)
                    || !ports.iter().any(|port| {
                        port.id() == data
                            && port.kind == PortKind::Data
                            && port.direction == state.direction
                            && port.schema == state.schema
                    })
                {
                    return Err(ContractError::InvalidStatePort {
                        state: state.id.clone(),
                        data: data.clone(),
                    });
                }
            }
        }
        Ok(Self {
            id,
            ports: ports.into(),
            plugin_identity: None,
            semantic_kind: None,
            output_progress: None,
        })
    }

    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    pub fn ports(&self) -> &[PortDescriptor] {
        &self.ports
    }

    /// The state lane for this data endpoint. The one-data/one-state shorthand
    /// is unambiguous; outputs with registered readers need no bootstrap port.
    pub fn state_port_for(&self, id: &PortId) -> Option<&PortDescriptor> {
        let data = self.ports.iter().find(|p| p.id() == id)?;
        if data.kind != PortKind::Data {
            return None;
        }
        let matching = || {
            self.ports
                .iter()
                .filter(|p| p.direction == data.direction && p.schema == data.schema)
        };
        if let Some(state) = matching().find(|p| p.state_for.as_ref() == Some(data.id())) {
            return Some(state);
        }
        if matching().filter(|p| p.kind == PortKind::Data).count() != 1 {
            return None;
        }
        let mut states =
            matching().filter(|p| p.kind == PortKind::Bootstrap && p.state_for.is_none());
        let state = states.next()?;
        states.next().is_none().then_some(state)
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

    /// Explicit output progress behavior. Sources default to an origin and
    /// transformers default to a relay when this is not declared.
    pub const fn output_progress(&self) -> Option<OutputProgress> {
        self.output_progress
    }

    pub fn with_output_progress(mut self, progress: OutputProgress) -> Self {
        self.output_progress = Some(progress);
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
    for (port, expected) in [
        (output, PortDirection::Output),
        (input, PortDirection::Input),
    ] {
        if port.direction != expected {
            return Err(ContractError::WrongPortDirection {
                port: port.id.clone(),
                expected,
                actual: port.direction,
            });
        }
    }
    if output.kind != input.kind {
        return Err(ContractError::PortKindMismatch {
            output: output.kind,
            input: input.kind,
        });
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

#[cfg(test)]
mod port_kind_tests {
    use super::*;

    fn port(direction: PortDirection, kind: PortKind) -> PortDescriptor {
        PortDescriptor::new(
            PortId::try_new("port").unwrap(),
            direction,
            crate::GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )
        .with_kind(kind)
    }

    #[test]
    fn data_kind_is_the_serde_default() -> anyhow::Result<()> {
        let data = port(PortDirection::Input, PortKind::Data);
        let encoded = serde_json::to_value(&data)?;
        assert!(encoded.get("kind").is_none());
        assert_eq!(serde_json::from_value::<PortDescriptor>(encoded)?, data);
        let bootstrap = port(PortDirection::Input, PortKind::Bootstrap);
        let encoded = serde_json::to_value(&bootstrap)?;
        assert_eq!(encoded["kind"], "Bootstrap");
        assert_eq!(
            serde_json::from_value::<PortDescriptor>(encoded)?,
            bootstrap
        );
        Ok(())
    }

    #[test]
    fn kinds_must_match_to_connect() {
        let pipe = PipeCapabilities::volatile_bounded(NonZeroUsize::new(1).unwrap());
        assert!(validate_connection(
            &port(PortDirection::Output, PortKind::Bootstrap),
            &port(PortDirection::Input, PortKind::Bootstrap),
            &pipe,
        )
        .is_ok());
        assert!(matches!(
            validate_connection(
                &port(PortDirection::Output, PortKind::Bootstrap),
                &port(PortDirection::Input, PortKind::Data),
                &pipe,
            ),
            Err(ContractError::PortKindMismatch { .. })
        ));
    }

    #[test]
    fn same_schema_inputs_do_not_share_another_ports_state_lane() -> anyhow::Result<()> {
        let id = ComponentId::try_new("30361edc-3edd-4da2-87a6-f229a3ac9baf")?;
        let data = |id: PortId| {
            PortDescriptor::new(
                id,
                PortDirection::Input,
                crate::GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )
        };
        let orders = PortId::try_new("orders")?;
        let audit = PortId::try_new("audit")?;
        let initial = port(PortDirection::Input, PortKind::Bootstrap).for_data_port(orders.clone());
        let descriptor = ComponentDescriptor::try_new(
            id.clone(),
            vec![data(orders.clone()), data(audit.clone()), initial.clone()],
        )?;
        let stored: ComponentDescriptor =
            serde_json::from_value(serde_json::to_value(&descriptor)?)?;
        assert_eq!(stored.state_port_for(&orders), Some(&initial));
        assert!(stored.state_port_for(&audit).is_none());
        assert!(stored.state_port_for(&PortId::try_new("absent")?).is_none());
        assert!(matches!(
            ComponentDescriptor::try_new(id, vec![data(audit), initial]),
            Err(ContractError::InvalidStatePort { .. })
        ));
        Ok(())
    }
}
