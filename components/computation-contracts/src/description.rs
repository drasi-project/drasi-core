//! Declared capabilities and requirements of a component instance.
//!
//! A [`ComponentDescription`] is what a component (or its factory, for one
//! specification and its bound resources) states about itself, in three
//! parts: component-level facts, per-port facts (including cross-port
//! relations) and per-dependency requirements on bound resources. Resources
//! state their own [`ResourceCapabilities`]; pipe providers state
//! [`PipeCapabilities`]. The graph compares declarations when it binds an
//! edge or a resource and never infers a missing fact: every fact is an
//! explicit value, and `Undeclared` satisfies no requirement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    ComponentDescriptor, DeliveryGuarantee, PipeCapabilities, PipeCapability, PortDirection, PortId,
};

/// Whether a component's own progress state survives a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgressPersistence {
    /// Not declared; satisfies no requirement.
    Undeclared,
    /// The component keeps no progress state of its own.
    Stateless,
    /// Progress is lost on reconstruction.
    Volatile,
    /// Progress survives reconstruction and process restart.
    Durable,
}

/// Component-level capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComponentCapabilities {
    pub progress: ProgressPersistence,
    /// What the graph does with a sink input that failed permanently. Only
    /// sinks act on it; other roles keep the default.
    #[serde(default)]
    pub on_permanent_failure: super::PermanentFailurePolicy,
}

/// The producer identity an output stream carries, as an identity-keyed
/// consumer input sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputIdentity {
    /// Not declared; satisfies no identity-keyed input.
    Undeclared,
    /// The output carries no producer identity for its own stream (a relay,
    /// or a source whose outputs carry only source replay progress).
    Untracked,
    /// A producer identity whose incarnation changes on every
    /// reconstruction; sequences do not survive a restart.
    Volatile,
    /// A restart-stable producer identity with persistent progress.
    Durable,
}

/// Whether an output stream can be replayed from a committed position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplayAbility {
    Undeclared,
    /// Nothing before the current position can be produced again.
    None,
    /// The producer replays from a committed position after a restart.
    FromPosition,
}

/// How an output relates to the component's other ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputRelation {
    /// `Relay` outputs carry their input's progress boundary forward instead
    /// of owning one.
    pub progress: super::OutputProgress,
    /// The output is published atomically with every other output that
    /// declares this relation.
    pub atomic_with_outputs: bool,
}

/// Declared facts of one output port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputDescription {
    pub identity: OutputIdentity,
    pub replay: ReplayAbility,
    pub relation: OutputRelation,
}

/// What an input requires of the producer identity it receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputDelivery {
    /// The input does not key its delivery on producer identity.
    Unkeyed,
    /// Exactly-once delivery keyed on a durable producer identity.
    ExactlyOnce,
    /// At-least-once delivery keyed on a volatile or durable identity.
    AtLeastOnce,
}

impl InputDelivery {
    pub fn guarantee(self) -> Option<DeliveryGuarantee> {
        match self {
            Self::Unkeyed => None,
            Self::ExactlyOnce => Some(DeliveryGuarantee::ExactlyOnce),
            Self::AtLeastOnce => Some(DeliveryGuarantee::AtLeastOnce),
        }
    }
}

impl From<DeliveryGuarantee> for InputDelivery {
    fn from(guarantee: DeliveryGuarantee) -> Self {
        match guarantee {
            DeliveryGuarantee::ExactlyOnce => Self::ExactlyOnce,
            DeliveryGuarantee::AtLeastOnce => Self::AtLeastOnce,
        }
    }
}

/// Declared requirements of one input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputDescription {
    pub delivery: InputDelivery,
}

/// The declared facts of one port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PortDescription {
    Input(InputDescription),
    Output(OutputDescription),
    /// A bootstrap input: what initialization the component accepts there.
    BootstrapInput(BootstrapInputDescription),
    /// A bootstrap output: what the snapshots served there carry.
    BootstrapOutput(BootstrapOutputDescription),
}

/// A versioned source/bootstrap coordination protocol: the typed contract
/// between a coordination-capable source and its paired providers. Shared
/// constants in the family's `*-common` crate name it; it is a declared
/// capability, never a plugin or implementation name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapProtocol {
    pub name: String,
    pub version: u32,
}

impl BootstrapProtocol {
    pub fn new(name: impl Into<String>, version: u32) -> Self {
        Self {
            name: name.into(),
            version,
        }
    }
}

impl std::fmt::Display for BootstrapProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// What a bootstrap output's sessions complete with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootstrapCompletionKind {
    /// Not declared; satisfies no requirement.
    Undeclared,
    /// Records only: no watermarks, no completion state, no progress
    /// binding, and preparation is always ready.
    Quiescent,
    /// May complete with source watermarks and completion state, and may
    /// bind the consumer's source progress.
    Durable,
    /// A relay (a source serving its provider's sessions): completes as the
    /// providers on the component's own bootstrap inputs do, or quiescently
    /// (an empty snapshot) when none is connected.
    Relayed,
}

/// Whose element namespace a provider's records use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ElementNamespace {
    /// Not declared; imposes no requirement.
    Undeclared,
    /// Records carry their own identities (a query's output snapshot).
    Own,
    /// Records are namespaced by the source whose bootstrap input the
    /// provider is connected to. The graph relation (the provider's edge to
    /// that source) supplies the source; the provider never names it.
    ServedSource,
}

/// How a component identifies the elements of one family, for comparing a
/// provider with the source it serves. `scheme` is a family-defined identity
/// scheme and `binding` its canonical description (selected tables and key
/// columns). Equal values produce equal element identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ElementIdentity {
    pub scheme: String,
    pub binding: String,
}

/// Declared facts of a bootstrap output port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapOutputDescription {
    pub completion: BootstrapCompletionKind,
    /// The coordination protocol the provider requires of the source it
    /// serves; `None` for an unpaired provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<BootstrapProtocol>,
    pub namespace: ElementNamespace,
    /// The element identities the provider produces, when they must equal
    /// the served source's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ElementIdentity>,
}

impl BootstrapOutputDescription {
    pub const UNDECLARED: Self = Self {
        completion: BootstrapCompletionKind::Undeclared,
        pairing: None,
        namespace: ElementNamespace::Undeclared,
        identity: None,
    };
    /// A relay's output (a source serving its provider's sessions).
    pub const RELAYED: Self = Self {
        completion: BootstrapCompletionKind::Relayed,
        pairing: None,
        namespace: ElementNamespace::Own,
        identity: None,
    };
    /// A committed-output snapshot (a query or transformer serving its own
    /// results): output-cursor watermarks under its own identities.
    pub const OUTPUT_SNAPSHOT: Self = Self {
        completion: BootstrapCompletionKind::Durable,
        pairing: None,
        namespace: ElementNamespace::Own,
        identity: None,
    };
}

/// What initialization a bootstrap input accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BootstrapAcceptance {
    /// Only quiescent providers (no watermarks, state or progress).
    Quiescent,
    /// Quiescent and durable providers.
    Durable,
}

/// Declared facts of a bootstrap input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapInputDescription {
    pub accepts: BootstrapAcceptance,
    /// The input belongs to a source that relays its consumers' sessions to
    /// the provider connected here, so the provider serves that source's
    /// element namespace.
    pub source_relay: bool,
}

impl BootstrapInputDescription {
    /// A source's bootstrap input.
    pub const SOURCE_RELAY: Self = Self {
        accepts: BootstrapAcceptance::Durable,
        source_relay: true,
    };
    /// A query's or durable transformer's bootstrap input.
    pub const DURABLE: Self = Self {
        accepts: BootstrapAcceptance::Durable,
        source_relay: false,
    };
    /// A quiescent transformer's bootstrap input.
    pub const QUIESCENT: Self = Self {
        accepts: BootstrapAcceptance::Quiescent,
        source_relay: false,
    };
}

/// A source's offer to coordinate bootstrap with paired providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapCoordination {
    pub protocol: BootstrapProtocol,
    /// Only a provider paired through `protocol` may be connected (the
    /// source admits no live data before its coordinated snapshot).
    pub required: bool,
}

/// Durability a resource declares about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceDurability {
    Undeclared,
    Volatile,
    Durable,
}

/// Capabilities a resource declares about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceCapabilities {
    pub durability: ResourceDurability,
}

impl ResourceCapabilities {
    pub const UNDECLARED: Self = Self {
        durability: ResourceDurability::Undeclared,
    };
    pub const VOLATILE: Self = Self {
        durability: ResourceDurability::Volatile,
    };
    pub const DURABLE: Self = Self {
        durability: ResourceDurability::Durable,
    };
}

/// Durability a component requires of the resources bound to one dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DurabilityRequirement {
    Any,
    Durable,
}

/// Requirements on every resource bound to one named dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DependencyRequirement {
    pub durability: DurabilityRequirement,
}

/// The declared capabilities of the resources bound to each named dependency
/// of one specification, as a factory sees them when it describes an instance.
pub type DependencyCapabilities = BTreeMap<String, Vec<ResourceCapabilities>>;

/// A component instance's declared capabilities and requirements.
///
/// Ports absent from the description are undeclared: an absent output
/// carries [`OutputIdentity::Undeclared`] and [`ReplayAbility::Undeclared`],
/// an absent input is [`InputDelivery::Unkeyed`]. Hosts that key delivery on
/// producer identity must refuse an instance whose identity-keyed input is
/// not declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComponentDescription {
    pub component: ComponentCapabilities,
    pub ports: BTreeMap<PortId, PortDescription>,
    pub dependencies: BTreeMap<String, DependencyRequirement>,
    /// A source's coordination offer to providers on its bootstrap input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordination: Option<BootstrapCoordination>,
    /// How a source identifies its elements, compared with providers that
    /// declare an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_identity: Option<ElementIdentity>,
}

impl ComponentDescription {
    /// A description that declares nothing: every fact is undeclared.
    pub fn undeclared() -> Self {
        Self {
            component: ComponentCapabilities {
                progress: ProgressPersistence::Undeclared,
                on_permanent_failure: super::PermanentFailurePolicy::Stall,
            },
            ports: BTreeMap::new(),
            dependencies: BTreeMap::new(),
            coordination: None,
            element_identity: None,
        }
    }

    pub fn new(progress: ProgressPersistence) -> Self {
        Self {
            component: ComponentCapabilities {
                progress,
                on_permanent_failure: super::PermanentFailurePolicy::Stall,
            },
            ..Self::undeclared()
        }
    }

    /// Declare what the graph does with a sink input that failed permanently.
    pub fn with_permanent_failure(mut self, policy: super::PermanentFailurePolicy) -> Self {
        self.component.on_permanent_failure = policy;
        self
    }

    /// Declare `description` on every data output port of `descriptor`.
    pub fn with_outputs(
        mut self,
        descriptor: &ComponentDescriptor,
        description: OutputDescription,
    ) -> Self {
        for port in descriptor.ports() {
            if port.direction() == PortDirection::Output && port.kind() == super::PortKind::Data {
                self.ports
                    .insert(port.id().clone(), PortDescription::Output(description));
            }
        }
        self
    }

    /// Declare `delivery` on every data input port of `descriptor`.
    pub fn with_inputs(
        mut self,
        descriptor: &ComponentDescriptor,
        delivery: InputDelivery,
    ) -> Self {
        for port in descriptor.ports() {
            if port.direction() == PortDirection::Input && port.kind() == super::PortKind::Data {
                self.ports.insert(
                    port.id().clone(),
                    PortDescription::Input(InputDescription { delivery }),
                );
            }
        }
        self
    }

    /// Declare the source's coordination offer.
    pub fn with_coordination(mut self, coordination: BootstrapCoordination) -> Self {
        self.coordination = Some(coordination);
        self
    }

    /// Declare how the source identifies its elements.
    pub fn with_element_identity(mut self, identity: ElementIdentity) -> Self {
        self.element_identity = Some(identity);
        self
    }

    /// Declare the facts of one port.
    pub fn with_port(mut self, port: PortId, description: PortDescription) -> Self {
        self.ports.insert(port, description);
        self
    }

    /// Declare `input` on every bootstrap input and `output` on every
    /// bootstrap output of `descriptor`.
    pub fn with_bootstrap_ports(
        mut self,
        descriptor: &ComponentDescriptor,
        input: BootstrapInputDescription,
        output: BootstrapOutputDescription,
    ) -> Self {
        for port in descriptor.ports() {
            if port.kind() != super::PortKind::Bootstrap {
                continue;
            }
            let description = match port.direction() {
                PortDirection::Input => PortDescription::BootstrapInput(input),
                PortDirection::Output => PortDescription::BootstrapOutput(output.clone()),
            };
            self.ports.insert(port.id().clone(), description);
        }
        self
    }

    pub fn with_dependency(
        mut self,
        name: impl Into<String>,
        requirement: DependencyRequirement,
    ) -> Self {
        self.dependencies.insert(name.into(), requirement);
        self
    }

    pub fn output(&self, port: &PortId) -> OutputDescription {
        match self.ports.get(port) {
            Some(PortDescription::Output(output)) => *output,
            _ => OutputDescription::UNDECLARED,
        }
    }

    pub fn input(&self, port: &PortId) -> InputDescription {
        match self.ports.get(port) {
            Some(PortDescription::Input(input)) => *input,
            _ => InputDescription {
                delivery: InputDelivery::Unkeyed,
            },
        }
    }

    /// The declared facts of a bootstrap output (undeclared when absent).
    pub fn bootstrap_output(&self, port: &PortId) -> BootstrapOutputDescription {
        match self.ports.get(port) {
            Some(PortDescription::BootstrapOutput(output)) => output.clone(),
            _ => BootstrapOutputDescription::UNDECLARED,
        }
    }

    /// The declared facts of a bootstrap input, if declared.
    pub fn bootstrap_input(&self, port: &PortId) -> Option<BootstrapInputDescription> {
        match self.ports.get(port) {
            Some(PortDescription::BootstrapInput(input)) => Some(*input),
            _ => None,
        }
    }

    /// Whether any input keys delivery on producer identity.
    pub fn has_keyed_inputs(&self) -> bool {
        self.ports.values().any(|port| {
            matches!(
                port,
                PortDescription::Input(InputDescription {
                    delivery: InputDelivery::ExactlyOnce | InputDelivery::AtLeastOnce
                })
            )
        })
    }

    /// Structural and internal-consistency check against the descriptor the
    /// description belongs to.
    pub fn validate(&self, descriptor: &ComponentDescriptor) -> std::result::Result<(), String> {
        for (id, port) in &self.ports {
            let declared = descriptor
                .ports()
                .iter()
                .find(|candidate| candidate.id() == id)
                .ok_or_else(|| format!("description declares unknown port '{id}'"))?;
            let bootstrap = declared.kind() == super::PortKind::Bootstrap;
            match (port, declared.direction()) {
                (PortDescription::Input(_), PortDirection::Input) if !bootstrap => {}
                (PortDescription::BootstrapInput(_), PortDirection::Input) if bootstrap => {}
                (PortDescription::BootstrapOutput(output), PortDirection::Output) if bootstrap => {
                    if output.pairing.is_some()
                        && (output.namespace != ElementNamespace::ServedSource
                            || output.completion != BootstrapCompletionKind::Durable)
                    {
                        return Err(format!(
                            "paired bootstrap output '{id}' must serve its source's namespace \
                             and complete durably"
                        ));
                    }
                }
                (PortDescription::Output(output), PortDirection::Output) if !bootstrap => {
                    output
                        .validate()
                        .map_err(|error| format!("output '{id}': {error}"))?;
                    if output.identity == OutputIdentity::Durable
                        && self.component.progress != ProgressPersistence::Durable
                    {
                        return Err(format!(
                            "output '{id}' declares a durable identity but the component's \
                             progress is {:?}",
                            self.component.progress
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "description of port '{id}' has the wrong direction or kind"
                    ))
                }
            }
        }
        Ok(())
    }
}

impl OutputDescription {
    pub const UNDECLARED: Self = Self {
        identity: OutputIdentity::Undeclared,
        replay: ReplayAbility::Undeclared,
        relation: OutputRelation::ORIGIN,
    };
    /// An origin output with a restart-stable identity that replays from a
    /// committed position.
    pub const DURABLE: Self = Self {
        identity: OutputIdentity::Durable,
        replay: ReplayAbility::FromPosition,
        relation: OutputRelation::ORIGIN,
    };
    /// An origin output whose identity changes on every reconstruction.
    pub const VOLATILE: Self = Self {
        identity: OutputIdentity::Volatile,
        replay: ReplayAbility::None,
        relation: OutputRelation::ORIGIN,
    };
    /// An origin output that carries no producer identity (for example a
    /// source whose outputs carry only source replay progress).
    pub const UNTRACKED: Self = Self {
        identity: OutputIdentity::Untracked,
        replay: ReplayAbility::None,
        relation: OutputRelation::ORIGIN,
    };
    /// A relay output: carries its input's progress boundary and no identity
    /// of its own.
    pub const RELAY: Self = Self {
        identity: OutputIdentity::Untracked,
        replay: ReplayAbility::None,
        relation: OutputRelation {
            progress: super::OutputProgress::Relay,
            atomic_with_outputs: false,
        },
    };

    /// The durable or volatile origin output matching a resource durability.
    pub fn following(durability: ResourceDurability) -> Self {
        match durability {
            ResourceDurability::Durable => Self::DURABLE,
            ResourceDurability::Volatile => Self::VOLATILE,
            ResourceDurability::Undeclared => Self::UNDECLARED,
        }
    }

    fn validate(&self) -> std::result::Result<(), String> {
        if self.relation.progress == super::OutputProgress::Relay
            && self.identity != OutputIdentity::Untracked
        {
            return Err("a relay output carries no producer identity of its own".into());
        }
        if self.replay == ReplayAbility::FromPosition && self.identity != OutputIdentity::Durable {
            return Err("replay from a position requires a durable identity".into());
        }
        Ok(())
    }
}

impl OutputRelation {
    pub const ORIGIN: Self = Self {
        progress: super::OutputProgress::Origin,
        atomic_with_outputs: false,
    };
}

/// Why two declarations cannot be connected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Incompatibility {
    #[error(
        "input requires {required:?} delivery keyed on producer identity, but the producer's \
         output identity is {identity:?}"
    )]
    ProducerIdentity {
        required: DeliveryGuarantee,
        identity: OutputIdentity,
    },
    #[error(
        "input requires exactly-once delivery, but neither the producer (replay {replay:?}) \
         nor the pipe can replay"
    )]
    Replay { replay: ReplayAbility },
    #[error(
        "dependency '{dependency}' requires {required:?} resources, but a bound resource \
         declares {declared:?}"
    )]
    ResourceDurability {
        dependency: String,
        required: DurabilityRequirement,
        declared: ResourceDurability,
    },
    #[error(
        "bootstrap provider requires source coordination {required}, but the connected \
         input offers {offered:?}"
    )]
    BootstrapPairing {
        required: Box<BootstrapProtocol>,
        offered: Option<Box<BootstrapProtocol>>,
    },
    #[error(
        "source requires a provider paired through {required}, but the connected provider is \
         unpaired"
    )]
    BootstrapCoordinationRequired { required: Box<BootstrapProtocol> },
    #[error(
        "bootstrap provider serves its source's element namespace and must be connected to a \
         source's bootstrap input"
    )]
    BootstrapNamespace,
    #[error(
        "bootstrap input accepts {accepts:?} initialization, but the connected provider \
         completes {completion:?}"
    )]
    BootstrapCompletion {
        accepts: BootstrapAcceptance,
        completion: BootstrapCompletionKind,
    },
    #[error(
        "bootstrap provider produces element identities {provided:?}, but the served source \
         declares {declared:?}"
    )]
    BootstrapIdentity {
        provided: Box<ElementIdentity>,
        declared: Option<Box<ElementIdentity>>,
    },
    #[error("instance description differs from its factory's declaration")]
    InstanceDescription,
    #[error("invalid component description: {0}")]
    InvalidDescription(String),
}

/// Check one edge: producer output × pipe × consumer input.
pub fn check_edge(
    output: OutputDescription,
    pipe: &PipeCapabilities,
    input: InputDescription,
) -> std::result::Result<(), Incompatibility> {
    let Some(required) = input.delivery.guarantee() else {
        return Ok(());
    };
    match (output.identity, required) {
        (OutputIdentity::Durable, _)
        | (OutputIdentity::Volatile, DeliveryGuarantee::AtLeastOnce) => {}
        (identity, required) => {
            return Err(Incompatibility::ProducerIdentity { required, identity })
        }
    }
    if required == DeliveryGuarantee::ExactlyOnce
        && output.replay != ReplayAbility::FromPosition
        && !pipe.supported().contains(&PipeCapability::Replay)
    {
        return Err(Incompatibility::Replay {
            replay: output.replay,
        });
    }
    Ok(())
}

/// The source whose namespace a provider on one bootstrap edge serves: the
/// consumer itself when it is a source relay, otherwise the single producer
/// of the consumer's data inputs (a query-local provider serves the source
/// the query reads). The graph supplies it from the edges it binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedNamespace<'a> {
    /// No single source: the consumer reads none, or several.
    Unavailable,
    /// The single source is known but not constructed yet; its declarations
    /// are compared when it is.
    Pending,
    /// The single source and its declarations.
    Source(&'a ComponentDescription),
}

/// Check one bootstrap edge: the provider's bootstrap output against the
/// consumer's bootstrap input and coordination offer.
///
/// A `Relayed` output must be resolved by the caller first (see
/// [`relayed_completion`]); an unresolved relay is treated as undeclared. An
/// undeclared input (`None`) accepts any completion but is no source relay.
pub fn check_bootstrap_edge(
    output: &BootstrapOutputDescription,
    consumer: &ComponentDescription,
    input: Option<BootstrapInputDescription>,
    served: ServedNamespace<'_>,
) -> std::result::Result<(), Incompatibility> {
    // An undeclared input states no acceptance requirement and is no source
    // relay, so it satisfies no provider requirement either.
    let input = input.unwrap_or(BootstrapInputDescription::DURABLE);
    let offered = consumer
        .coordination
        .as_ref()
        .filter(|_| input.source_relay);
    match (&output.pairing, offered) {
        (Some(required), Some(offer)) if &offer.protocol == required => {}
        (Some(required), offer) => {
            return Err(Incompatibility::BootstrapPairing {
                required: Box::new(required.clone()),
                offered: offer.map(|offer| Box::new(offer.protocol.clone())),
            })
        }
        (None, Some(offer)) if offer.required => {
            return Err(Incompatibility::BootstrapCoordinationRequired {
                required: Box::new(offer.protocol.clone()),
            })
        }
        (None, _) => {}
    }
    if output.namespace == ElementNamespace::ServedSource && served == ServedNamespace::Unavailable
    {
        return Err(Incompatibility::BootstrapNamespace);
    }
    if let Some(identity) = &output.identity {
        let declared = match served {
            ServedNamespace::Source(source) => source.element_identity.as_ref(),
            ServedNamespace::Pending => Some(identity),
            ServedNamespace::Unavailable => None,
        };
        if declared != Some(identity) {
            return Err(Incompatibility::BootstrapIdentity {
                provided: Box::new(identity.clone()),
                declared: declared.cloned().map(Box::new),
            });
        }
    }
    let completion = match output.completion {
        BootstrapCompletionKind::Relayed => BootstrapCompletionKind::Undeclared,
        completion => completion,
    };
    match (input.accepts, completion) {
        (BootstrapAcceptance::Durable, _)
        | (BootstrapAcceptance::Quiescent, BootstrapCompletionKind::Quiescent) => Ok(()),
        (accepts, completion) => Err(Incompatibility::BootstrapCompletion {
            accepts,
            completion,
        }),
    }
}

/// The completion a relay's consumers observe, given the completions of the
/// providers connected to the relay's bootstrap inputs: quiescent with none,
/// otherwise the weakest guarantee among them (`Undeclared` dominates, then
/// `Durable`). An unresolved nested relay counts as undeclared.
pub fn relayed_completion(
    upstream: impl IntoIterator<Item = BootstrapCompletionKind>,
) -> BootstrapCompletionKind {
    use BootstrapCompletionKind::*;
    upstream
        .into_iter()
        .fold(Quiescent, |current, next| match (current, next) {
            (Undeclared, _) | (_, Undeclared) | (_, Relayed) | (Relayed, _) => Undeclared,
            (Durable, _) | (_, Durable) => Durable,
            (Quiescent, Quiescent) => Quiescent,
        })
}

/// Check one resource bound to a named dependency against the requirement
/// the component declares for it. Dependencies without a declared
/// requirement accept any resource.
pub fn check_dependency(
    description: &ComponentDescription,
    dependency: &str,
    resource: ResourceCapabilities,
) -> std::result::Result<(), Incompatibility> {
    let Some(requirement) = description.dependencies.get(dependency) else {
        return Ok(());
    };
    match (requirement.durability, resource.durability) {
        (DurabilityRequirement::Any, _)
        | (DurabilityRequirement::Durable, ResourceDurability::Durable) => Ok(()),
        (required, declared) => Err(Incompatibility::ResourceDurability {
            dependency: dependency.to_owned(),
            required,
            declared,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipe(replay: bool) -> PipeCapabilities {
        use PipeCapability::*;
        if replay {
            PipeCapabilities::try_new([DurableAcceptance, ExplicitAcknowledgement, Replay], None)
                .unwrap()
        } else {
            PipeCapabilities::try_new([], None).unwrap()
        }
    }

    fn input(delivery: InputDelivery) -> InputDescription {
        InputDescription { delivery }
    }

    #[test]
    fn edges_compare_identity_replay_and_pipe() {
        use InputDelivery::*;
        let outputs = [
            OutputDescription::DURABLE,
            OutputDescription::VOLATILE,
            OutputDescription::UNTRACKED,
            OutputDescription::RELAY,
            OutputDescription::UNDECLARED,
        ];
        for output in outputs {
            assert_eq!(check_edge(output, &pipe(false), input(Unkeyed)), Ok(()));
        }
        assert_eq!(
            check_edge(OutputDescription::DURABLE, &pipe(false), input(ExactlyOnce)),
            Ok(())
        );
        assert_eq!(
            check_edge(OutputDescription::DURABLE, &pipe(false), input(AtLeastOnce)),
            Ok(())
        );
        assert_eq!(
            check_edge(
                OutputDescription::VOLATILE,
                &pipe(false),
                input(AtLeastOnce)
            ),
            Ok(())
        );
        for output in [
            OutputDescription::VOLATILE,
            OutputDescription::UNTRACKED,
            OutputDescription::UNDECLARED,
        ] {
            assert!(matches!(
                check_edge(output, &pipe(true), input(ExactlyOnce)),
                Err(Incompatibility::ProducerIdentity { .. })
            ));
        }
        for output in [
            OutputDescription::UNTRACKED,
            OutputDescription::RELAY,
            OutputDescription::UNDECLARED,
        ] {
            assert!(check_edge(output, &pipe(true), input(AtLeastOnce)).is_err());
        }
        // A durable identity without producer replay needs a replaying pipe.
        let unreplayable = OutputDescription {
            replay: ReplayAbility::None,
            ..OutputDescription::DURABLE
        };
        assert_eq!(
            check_edge(unreplayable, &pipe(false), input(ExactlyOnce)),
            Err(Incompatibility::Replay {
                replay: ReplayAbility::None
            })
        );
        assert_eq!(
            check_edge(unreplayable, &pipe(true), input(ExactlyOnce)),
            Ok(())
        );
    }

    /// The edge check as the graph applies it: a relay serves its own
    /// namespace; another consumer here reads no single source.
    fn check(
        output: &BootstrapOutputDescription,
        consumer: &ComponentDescription,
        input: Option<BootstrapInputDescription>,
    ) -> std::result::Result<(), Incompatibility> {
        let served = if input.is_some_and(|input| input.source_relay) {
            ServedNamespace::Source(consumer)
        } else {
            ServedNamespace::Unavailable
        };
        check_bootstrap_edge(output, consumer, input, served)
    }

    #[test]
    fn a_query_local_provider_serves_the_single_source_the_query_reads() {
        let local = BootstrapOutputDescription {
            completion: BootstrapCompletionKind::Quiescent,
            pairing: None,
            namespace: ElementNamespace::ServedSource,
            identity: Some(ElementIdentity {
                scheme: "drasi.test/elements".into(),
                binding: "items".into(),
            }),
        };
        let query = ComponentDescription::new(ProgressPersistence::Durable);
        let input = Some(BootstrapInputDescription::DURABLE);
        let read = ComponentDescription::new(ProgressPersistence::Durable).with_element_identity(
            ElementIdentity {
                scheme: "drasi.test/elements".into(),
                binding: "items".into(),
            },
        );
        assert_eq!(
            check_bootstrap_edge(&local, &query, input, ServedNamespace::Source(&read)),
            Ok(())
        );
        assert_eq!(
            check_bootstrap_edge(&local, &query, input, ServedNamespace::Pending),
            Ok(()),
            "a source that is not constructed yet is compared when it is"
        );
        assert_eq!(
            check_bootstrap_edge(&local, &query, input, ServedNamespace::Unavailable),
            Err(Incompatibility::BootstrapNamespace),
            "a query reading no single source gives the provider no namespace"
        );
        let other = ComponentDescription::new(ProgressPersistence::Durable);
        assert!(matches!(
            check_bootstrap_edge(&local, &query, input, ServedNamespace::Source(&other)),
            Err(Incompatibility::BootstrapIdentity { declared: None, .. })
        ));
    }

    fn protocol(name: &str) -> BootstrapProtocol {
        BootstrapProtocol::new(name, 1)
    }

    fn provider(
        pairing: Option<BootstrapProtocol>,
        completion: BootstrapCompletionKind,
    ) -> BootstrapOutputDescription {
        BootstrapOutputDescription {
            completion,
            pairing,
            namespace: ElementNamespace::ServedSource,
            identity: None,
        }
    }

    fn source(coordination: Option<BootstrapCoordination>) -> ComponentDescription {
        ComponentDescription {
            coordination,
            ..ComponentDescription::new(ProgressPersistence::Durable)
        }
    }

    #[test]
    fn paired_provider_requires_the_same_coordination_protocol() {
        use BootstrapCompletionKind::Durable;
        let offer = |name: &str, required| BootstrapCoordination {
            protocol: protocol(name),
            required,
        };
        let relay = Some(BootstrapInputDescription::SOURCE_RELAY);
        let paired = provider(Some(protocol("drasi.test/coordinated")), Durable);
        assert_eq!(
            check(
                &paired,
                &source(Some(offer("drasi.test/coordinated", true))),
                relay
            ),
            Ok(())
        );
        // Another protocol, or none at all, is refused when the edge binds.
        assert_eq!(
            check(
                &paired,
                &source(Some(offer("drasi.test/other", false))),
                relay
            ),
            Err(Incompatibility::BootstrapPairing {
                required: Box::new(protocol("drasi.test/coordinated")),
                offered: Some(Box::new(protocol("drasi.test/other"))),
            })
        );
        assert!(matches!(
            check(&paired, &source(None), relay),
            Err(Incompatibility::BootstrapPairing { offered: None, .. })
        ));
        // A coordination offer on a query (not a source relay) does not count.
        assert!(matches!(
            check(
                &paired,
                &source(Some(offer("drasi.test/coordinated", true))),
                Some(BootstrapInputDescription::DURABLE)
            ),
            Err(Incompatibility::BootstrapPairing { .. })
        ));
        // A source that requires coordination refuses an unpaired provider;
        // an optional offer accepts it.
        let unpaired = provider(None, BootstrapCompletionKind::Quiescent);
        assert_eq!(
            check(
                &unpaired,
                &source(Some(offer("drasi.test/coordinated", true))),
                relay
            ),
            Err(Incompatibility::BootstrapCoordinationRequired {
                required: Box::new(protocol("drasi.test/coordinated"))
            })
        );
        assert_eq!(
            check(
                &unpaired,
                &source(Some(offer("drasi.test/coordinated", false))),
                relay
            ),
            Ok(())
        );
    }

    #[test]
    fn served_source_namespace_requires_a_source_relay() {
        let unpaired = provider(None, BootstrapCompletionKind::Quiescent);
        assert_eq!(
            check(
                &unpaired,
                &source(None),
                Some(BootstrapInputDescription::DURABLE)
            ),
            Err(Incompatibility::BootstrapNamespace)
        );
        let own = BootstrapOutputDescription {
            namespace: ElementNamespace::Own,
            ..unpaired.clone()
        };
        assert_eq!(
            check(
                &own,
                &source(None),
                Some(BootstrapInputDescription::DURABLE)
            ),
            Ok(())
        );
        assert_eq!(check(&own, &source(None), None), Ok(()));
        assert_eq!(
            check(&unpaired, &source(None), None),
            Err(Incompatibility::BootstrapNamespace),
            "an undeclared input is no source relay"
        );
    }

    #[test]
    fn quiescent_inputs_refuse_durable_relayed_and_undeclared_providers() {
        use BootstrapCompletionKind::*;
        let own = |completion| BootstrapOutputDescription {
            completion,
            pairing: None,
            namespace: ElementNamespace::Own,
            identity: None,
        };
        let quiescent = Some(BootstrapInputDescription::QUIESCENT);
        assert_eq!(check(&own(Quiescent), &source(None), quiescent), Ok(()));
        for completion in [Durable, Undeclared, Relayed] {
            assert_eq!(
                check(&own(completion), &source(None), quiescent),
                Err(Incompatibility::BootstrapCompletion {
                    accepts: BootstrapAcceptance::Quiescent,
                    completion: if completion == Relayed {
                        Undeclared
                    } else {
                        completion
                    },
                })
            );
        }
        for completion in [Quiescent, Durable, Undeclared] {
            assert_eq!(
                check(
                    &own(completion),
                    &source(None),
                    Some(BootstrapInputDescription::DURABLE)
                ),
                Ok(())
            );
        }
        assert_eq!(relayed_completion([]), Quiescent);
        assert_eq!(relayed_completion([Quiescent, Quiescent]), Quiescent);
        assert_eq!(relayed_completion([Quiescent, Durable]), Durable);
        assert_eq!(relayed_completion([Durable, Undeclared]), Undeclared);
        assert_eq!(relayed_completion([Relayed]), Undeclared);
    }

    #[test]
    fn provider_identity_must_equal_the_served_sources() {
        let identity = |binding: &str| ElementIdentity {
            scheme: "drasi.test/elements".into(),
            binding: binding.into(),
        };
        let file = BootstrapOutputDescription {
            identity: Some(identity("[sensors:serial]")),
            ..provider(None, BootstrapCompletionKind::Quiescent)
        };
        let relay = Some(BootstrapInputDescription::SOURCE_RELAY);
        let matching = source(None).with_element_identity(identity("[sensors:serial]"));
        assert_eq!(check(&file, &matching, relay), Ok(()));
        let rowid = source(None).with_element_identity(identity("[sensors:rowid]"));
        assert!(matches!(
            check(&file, &rowid, relay),
            Err(Incompatibility::BootstrapIdentity { .. })
        ));
        assert!(matches!(
            check(&file, &source(None), relay),
            Err(Incompatibility::BootstrapIdentity { declared: None, .. })
        ));
    }

    #[test]
    fn dependencies_compare_declared_resource_durability() {
        let description = ComponentDescription::new(ProgressPersistence::Durable).with_dependency(
            "indexes",
            DependencyRequirement {
                durability: DurabilityRequirement::Durable,
            },
        );
        assert_eq!(
            check_dependency(&description, "indexes", ResourceCapabilities::DURABLE),
            Ok(())
        );
        for resource in [
            ResourceCapabilities::VOLATILE,
            ResourceCapabilities::UNDECLARED,
        ] {
            assert!(check_dependency(&description, "indexes", resource).is_err());
        }
        assert_eq!(
            check_dependency(&description, "other", ResourceCapabilities::UNDECLARED),
            Ok(())
        );
    }

    #[test]
    fn descriptions_round_trip_and_validate_against_ports() {
        use crate::{PipeRequirements, PortDescriptor};
        let schema = crate::GraphChangeCodec::schema().descriptor().clone();
        let descriptor = ComponentDescriptor::try_new(
            crate::ComponentId::try_new("00000000-0000-4000-8000-000000000001").unwrap(),
            vec![
                PortDescriptor::new(
                    PortId::try_new("in").unwrap(),
                    PortDirection::Input,
                    schema.clone(),
                    PipeRequirements::default(),
                ),
                PortDescriptor::new(
                    PortId::try_new("out").unwrap(),
                    PortDirection::Output,
                    schema,
                    PipeRequirements::default(),
                ),
            ],
        )
        .unwrap();
        let description = ComponentDescription::new(ProgressPersistence::Durable)
            .with_inputs(&descriptor, InputDelivery::AtLeastOnce)
            .with_outputs(&descriptor, OutputDescription::DURABLE);
        description.validate(&descriptor).unwrap();
        let decoded: ComponentDescription =
            serde_json::from_value(serde_json::to_value(&description).unwrap()).unwrap();
        assert_eq!(decoded, description);
        let volatile = ComponentDescription::new(ProgressPersistence::Volatile)
            .with_outputs(&descriptor, OutputDescription::DURABLE);
        assert!(volatile.validate(&descriptor).is_err());
        let relay = ComponentDescription::new(ProgressPersistence::Stateless).with_outputs(
            &descriptor,
            OutputDescription {
                identity: OutputIdentity::Volatile,
                ..OutputDescription::RELAY
            },
        );
        assert!(relay.validate(&descriptor).is_err());
        assert_eq!(
            ComponentDescription::undeclared().output(&PortId::try_new("out").unwrap()),
            OutputDescription::UNDECLARED
        );
    }
}
