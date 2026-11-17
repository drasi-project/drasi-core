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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PortDescription {
    Input(InputDescription),
    Output(OutputDescription),
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

    /// Declare `description` on every output port of `descriptor`.
    pub fn with_outputs(
        mut self,
        descriptor: &ComponentDescriptor,
        description: OutputDescription,
    ) -> Self {
        for port in descriptor.ports() {
            if port.direction() == PortDirection::Output {
                self.ports
                    .insert(port.id().clone(), PortDescription::Output(description));
            }
        }
        self
    }

    /// Declare `delivery` on every input port of `descriptor`.
    pub fn with_inputs(
        mut self,
        descriptor: &ComponentDescriptor,
        delivery: InputDelivery,
    ) -> Self {
        for port in descriptor.ports() {
            if port.direction() == PortDirection::Input {
                self.ports.insert(
                    port.id().clone(),
                    PortDescription::Input(InputDescription { delivery }),
                );
            }
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
            match (port, declared.direction()) {
                (PortDescription::Input(_), PortDirection::Input) => {}
                (PortDescription::Output(output), PortDirection::Output) => {
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
                        "description of port '{id}' has the wrong direction"
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
