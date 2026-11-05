// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Serializable discovery contracts. These are wire data, not ABI structs.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, ComponentSemanticKind, ImplementationIdentity,
    OutputProgress, PluginIdentity, PortDescriptor, PortDirection, PortFormatAlternative, PortId,
    PortPrimaries, SchemaDescriptor, SinkCompletion,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::abi;

pub const TARGET: &str = env!("DRASI_COMPUTATION_TARGET");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigType {
    Boolean,
    Integer,
    String,
    Object,
    Array,
    Json,
}

impl ConfigType {
    pub(crate) fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Boolean => value.is_boolean(),
            Self::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            Self::String => value.is_string(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::Json => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigField {
    pub value_type: ConfigType,
    pub required: bool,
    /// Host construction requires an unresolved secret reference, never a literal.
    pub secret: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSchema {
    pub fields: BTreeMap<String, ConfigField>,
    pub allow_additional: bool,
}

impl ConfigSchema {
    /// Builds a closed schema with non-secret fields.
    pub fn from_fields(fields: impl IntoIterator<Item = (&'static str, ConfigType, bool)>) -> Self {
        Self {
            fields: fields
                .into_iter()
                .map(|(name, value_type, required)| {
                    (
                        name.to_owned(),
                        ConfigField {
                            value_type,
                            required,
                            secret: false,
                        },
                    )
                })
                .collect(),
            allow_additional: false,
        }
    }

    pub fn validate(&self, configuration: &Value) -> anyhow::Result<()> {
        let object = configuration
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("native component configuration must be an object"))?;
        for (name, field) in &self.fields {
            anyhow::ensure!(
                !field.required || object.contains_key(name),
                "missing native configuration field {name}"
            );
        }
        for (name, value) in object {
            ComponentId::try_new(name.as_str())?;
            match self.fields.get(name) {
                Some(field) => anyhow::ensure!(
                    field.value_type.accepts(value),
                    "native configuration field {name} has an incompatible type"
                ),
                None => anyhow::ensure!(
                    self.allow_additional,
                    "unknown native configuration field {name}"
                ),
            }
        }
        Ok(())
    }
}

/// Only implemented capabilities have fields. Unknown fields are rejected, not
/// ignored. Optional query read semantics use the output-read extension; they
/// do not introduce a query execution role or expose host runtime objects.
///
/// `snapshot_inputs` lists the sink input ports whose registered handler can
/// replace consumer state from a snapshot. It must match the constructed sink's
/// actual per-port handlers exactly; an empty list means no input supports it.
///
/// `output_snapshots` and `output_replays` list the output ports for which the
/// constructed component registered a snapshot or replay provider
/// (`NativeComponent::output_readers`). Each list must match the actual
/// registrations exactly and the two are independent; empty lists are omitted
/// from the manifest. `output_follows` lists the output ports whose constructed
/// component registered a snapshot-follow provider (bounded lease: coherent
/// snapshot plus live tail); it is independent of the other two and needs the
/// separately negotiated output-follow extension.
///
/// `port_formats` lists the non-default schemas each existing port actually
/// implements; `FactoryMetadata::ports` stay the defaults. Configuration selects
/// one per port through the optional object field `ports`
/// (`{"out": {"format": "<schema id>-v<version>"}}`). Every alternative schema
/// must also be in the plugin's `schemas`, and the plugin must construct a
/// component whose descriptor matches the selection
/// (`FactoryMetadata::for_configuration`).
///
/// `binds_inputs` and `input_binding_updates` are only valid for a sink or
/// transformer, which have inputs. The former supplies construction-time
/// bindings and triggers reconstruction when they change; the latter delivers
/// current bindings to the existing component at operation boundaries.
///
/// `primary_ports` optionally designates one primary input and one primary
/// output by port id, for shorthand endpoints that name a component without a
/// port. Each must name an existing port of that direction (checked in
/// `FactoryMetadata::validate`); it is unaffected by format selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub control: bool,
    pub readiness: bool,
    pub transactional: bool,
    pub continuations: bool,
    /// Poll the transformer's continuation once after successful startup,
    /// before admitting input, so it can replay recovered output.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub startup_continuation: bool,
    pub wakeups: bool,
    pub snapshot_inputs: Vec<PortId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_snapshots: Vec<PortId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_replays: Vec<PortId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_follows: Vec<PortId>,
    /// Optional query read semantics over declared outputs, not an execution role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_queries: Vec<PortId>,
    /// Include canonical output-state observations in query transform replies.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub query_observations: bool,
    /// Retained recovery reads after positively completed query cleanup.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub query_recovery_views: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_formats: Vec<PortFormatAlternative>,
    #[serde(default, skip_serializing_if = "PortPrimaries::is_empty")]
    pub primary_ports: PortPrimaries,
    /// The component consumes `CreateRequest::inputs`. The graph then holds
    /// its creation until every incoming output has a declared stream and
    /// reconstructs it when those bindings change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binds_inputs: bool,
    /// The component receives the graph's current incoming bindings before
    /// start and processing, without requiring construction-time bindings.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub input_binding_updates: bool,
    /// The transformer implements retryable, explicit deletion of its own
    /// state after successful cleanup. Other roles cannot declare this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deprovision: bool,
    /// The transformer requires the host-owned indexed processing service.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub indexed_processing: bool,
    /// Transaction-owned requests and committed deadline observations through
    /// ScheduledProcessingV1. Requires atomic persistent processing resources.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub scheduled_processing: bool,
    /// Also implements an explicitly volatile provider profile. The host must
    /// not grant atomic output or durable bootstrap guarantees to that profile.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub volatile_processing: bool,
    /// Also implements explicitly non-atomic persistent output publication.
    /// This does not grant atomic state/output or shared-group guarantees.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub non_atomic_publication: bool,
    /// Persistent delivery progress and actual Host-owned output destinations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub durable_output: bool,
    /// Committed progress published by the input's actual Host processing owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_input_progress: Option<PortId>,
    /// Provider-driven initialization on this input before processing or reads.
    /// Only quiescent, non-progress-bound providers are supported by this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiescent_bootstrap: Option<PortId>,
    /// The quiescent consumer reports whether its configuration requests a
    /// provider; ordinary instances can start without allocating bootstrap state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bootstrap_optional: bool,
    /// Optional provider-driven initialization with atomic completion and the
    /// actual processing owner's durable state/progress services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_bootstrap: Option<PortId>,
    /// Optional semantic identity, independent of execution role. A
    /// `Transformer` may declare `Query`; the annotation never selects a
    /// different scheduler, storage path or evaluator owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_kind: Option<ComponentSemanticKind>,
    /// Optional output progress behavior, independent of semantic identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_progress: Option<OutputProgress>,
}

impl Capabilities {
    pub fn declares_output_reads(&self) -> bool {
        !self.output_snapshots.is_empty()
            || !self.output_replays.is_empty()
            || !self.output_queries.is_empty()
    }

    pub fn declares_output_follows(&self) -> bool {
        !self.output_follows.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryMetadata {
    pub implementation: ImplementationIdentity,
    pub role: ComponentRole,
    pub configuration_version: u32,
    pub configuration: ConfigSchema,
    pub ports: Vec<PortDescriptor>,
    pub completion: Option<SinkCompletion>,
    pub capabilities: Capabilities,
}

impl FactoryMetadata {
    pub fn descriptor(&self, id: ComponentId) -> anyhow::Result<ComponentDescriptor> {
        let mut descriptor = ComponentDescriptor::try_new(id, self.ports.clone())?;
        if let Some(kind) = self.capabilities.semantic_kind {
            descriptor = descriptor.with_semantic_kind(kind);
        }
        if let Some(progress) = self.capabilities.output_progress {
            descriptor = descriptor.with_output_progress(progress);
        }
        match &self.implementation.plugin {
            Some(plugin) => Ok(descriptor.with_plugin_identity(plugin.clone())?),
            None => Ok(descriptor),
        }
    }

    /// The semantic kind of this factory's instances.
    pub fn semantic_kind(&self) -> ComponentSemanticKind {
        self.capabilities
            .semantic_kind
            .unwrap_or_else(|| ComponentSemanticKind::from(self.role))
    }

    pub fn validate(
        &self,
        plugin: &PluginIdentity,
        schemas: &[SchemaDescriptor],
    ) -> anyhow::Result<()> {
        ComponentId::try_new(self.implementation.name.as_ref())?;
        ComponentId::try_new(self.implementation.version.as_ref())?;
        anyhow::ensure!(
            self.implementation.plugin.as_ref() == Some(plugin),
            "native factory plugin provenance does not match its manifest"
        );
        anyhow::ensure!(
            self.configuration_version > 0,
            "invalid native configuration version"
        );
        for name in self.configuration.fields.keys() {
            ComponentId::try_new(name.as_str())?;
        }
        self.descriptor(ComponentId::try_new("validation")?)?;
        let inputs = self
            .ports
            .iter()
            .filter(|p| p.direction() == PortDirection::Input)
            .count();
        let outputs = self.ports.len() - inputs;
        anyhow::ensure!(
            match self.role {
                ComponentRole::Source => inputs == 0 && outputs > 0,
                ComponentRole::Transformer => inputs > 0 && outputs > 0,
                ComponentRole::Sink => inputs > 0 && outputs == 0,
                ComponentRole::Service => inputs == 0 && outputs == 0,
            },
            "unsupported native role or incompatible ports"
        );
        if let Some(kind) = self.capabilities.semantic_kind {
            anyhow::ensure!(
                matches!(
                    (self.role, kind),
                    (ComponentRole::Transformer, ComponentSemanticKind::Query)
                ) || kind == ComponentSemanticKind::from(self.role),
                "native factory declares semantic kind {kind:?} that its {:?} role cannot carry",
                self.role
            );
        }
        if let Some(progress) = self.capabilities.output_progress {
            anyhow::ensure!(
                outputs > 0
                    && (self.role != ComponentRole::Source || progress == OutputProgress::Origin),
                "native factory declares invalid output progress for its role"
            );
        }
        anyhow::ensure!(
            (self.role == ComponentRole::Sink) == self.completion.is_some(),
            "only native sinks must declare a completion boundary"
        );
        for port in &self.ports {
            anyhow::ensure!(
                schemas.iter().any(|schema| schema == port.schema()),
                "native port {} uses an undeclared or conflicting schema",
                port.id()
            );
            if let Some(completion) = self.completion {
                drasi_computation_contracts::validate_sink_completion(
                    completion,
                    port.requirements(),
                )?;
            }
        }
        self.validate_port_formats(schemas)?;
        let c = &self.capabilities;
        anyhow::ensure!(
            !c.readiness || c.control,
            "native readiness requires the control interface"
        );
        anyhow::ensure!(
            !c.binds_inputs
                || matches!(self.role, ComponentRole::Sink | ComponentRole::Transformer),
            "binding inputs requires a native sink or transformer"
        );
        anyhow::ensure!(
            !c.input_binding_updates
                || matches!(self.role, ComponentRole::Sink | ComponentRole::Transformer),
            "input-binding updates require a native sink or transformer"
        );
        anyhow::ensure!(
            !c.deprovision || self.role == ComponentRole::Transformer,
            "native deprovision currently requires a transformer"
        );
        anyhow::ensure!(
            !c.indexed_processing || (self.role == ComponentRole::Transformer && !c.transactional),
            "native indexed processing requires a non-participant transformer"
        );
        anyhow::ensure!(
            !c.volatile_processing
                || (c.indexed_processing && c.committed_input_progress.is_some()),
            "volatile indexed processing requires its own input-progress publication"
        );
        anyhow::ensure!(
            !c.scheduled_processing
                || (c.indexed_processing
                    && c.committed_input_progress.is_some()
                    && c.wakeups
                    && !c.volatile_processing
                    && !c.non_atomic_publication),
            "scheduled processing requires atomic indexed progress and wakeups"
        );
        if let Some(input) = &c.committed_input_progress {
            anyhow::ensure!(
                c.indexed_processing
                    && self.ports.iter().any(|port| {
                        port.direction() == PortDirection::Input && port.id() == input
                    }),
                "committed input progress requires a declared indexed input"
            );
        }
        anyhow::ensure!(
            !c.durable_output
                || (c.indexed_processing
                    && c.committed_input_progress.is_some()
                    && c.startup_continuation),
            "durable output requires indexed progress and startup replay"
        );
        anyhow::ensure!(
            !c.non_atomic_publication || c.durable_output,
            "non-atomic publication requires indexed progress and durable output"
        );
        if let Some(input) = &c.quiescent_bootstrap {
            anyhow::ensure!(
                self.role == ComponentRole::Transformer
                    && !c.transactional
                    && !c.indexed_processing
                    && self.ports.iter().any(|port| {
                        port.direction() == PortDirection::Input && port.id() == input
                    }),
                "quiescent bootstrap requires a non-indexed, non-participant transformer input"
            );
        }
        if let Some(input) = &c.durable_bootstrap {
            anyhow::ensure!(
                c.quiescent_bootstrap.is_none()
                    && c.indexed_processing
                    && c.committed_input_progress.as_ref() == Some(input),
                "durable bootstrap requires the indexed input's committed progress"
            );
        }
        anyhow::ensure!(
            !c.bootstrap_optional || c.quiescent_bootstrap.is_some(),
            "optional bootstrap requires a quiescent bootstrap input"
        );
        anyhow::ensure!(
            c.snapshot_inputs.is_empty() || self.role == ComponentRole::Sink,
            "snapshot capability requires a native sink"
        );
        let mut snapshot_inputs = BTreeSet::new();
        for port in &c.snapshot_inputs {
            anyhow::ensure!(
                snapshot_inputs.insert(port),
                "duplicate native snapshot input {port}"
            );
            anyhow::ensure!(
                self.ports
                    .iter()
                    .any(|p| p.id() == port && p.direction() == PortDirection::Input),
                "native snapshot input {port} is not a declared input port"
            );
        }
        for (kind, ports) in [
            ("snapshot", &c.output_snapshots),
            ("replay", &c.output_replays),
            ("follow", &c.output_follows),
            ("query", &c.output_queries),
        ] {
            let mut declared = BTreeSet::new();
            for port in ports {
                anyhow::ensure!(
                    declared.insert(port),
                    "duplicate native output {kind} port {port}"
                );
                anyhow::ensure!(
                    self.ports
                        .iter()
                        .any(|p| p.id() == port && p.direction() == PortDirection::Output),
                    "native output {kind} port {port} is not a declared output port"
                );
            }
        }
        anyhow::ensure!(
            c.output_queries.is_empty() || self.semantic_kind() == ComponentSemanticKind::Query,
            "query read semantics require a declared query component"
        );
        anyhow::ensure!(
            !c.query_observations
                || (self.role == ComponentRole::Transformer && c.output_queries.len() == 1),
            "query observations require one registered query output on a transformer"
        );
        anyhow::ensure!(
            !c.query_recovery_views || c.query_observations,
            "query recovery views require canonical query observations"
        );
        anyhow::ensure!(
            !(c.continuations || c.wakeups || c.transactional)
                || self.role == ComponentRole::Transformer,
            "transformer capability on another role"
        );
        anyhow::ensure!(
            !c.startup_continuation || c.continuations,
            "startup continuation requires transformer continuations"
        );
        anyhow::ensure!(!c.transactional || (inputs == 1 && outputs == 1
            && !c.wakeups && !c.continuations && !c.control && !c.readiness),
            "transaction participants must be linear and cannot own control, wakeups or continuations");
        Ok(())
    }

    pub fn validate_instance(
        &self,
        descriptor: &ComponentDescriptor,
        id: &ComponentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            descriptor.id() == id && descriptor.ports() == self.ports,
            "native component descriptor differs from its factory contract"
        );
        anyhow::ensure!(
            descriptor.plugin_identity().is_none()
                || descriptor.plugin_identity() == self.implementation.plugin.as_ref(),
            "native component changed plugin identity"
        );
        anyhow::ensure!(
            descriptor.semantic_kind().is_none()
                || descriptor.semantic_kind() == Some(self.semantic_kind()),
            "native component claims unsupported semantic role"
        );
        anyhow::ensure!(
            descriptor.output_progress().is_none()
                || descriptor.output_progress() == self.capabilities.output_progress,
            "native component claims unsupported output progress"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMetadata {
    pub abi_version: String,
    pub wire_version: u32,
    pub plugin: PluginIdentity,
    pub factories: Vec<FactoryMetadata>,
    pub schemas: Vec<SchemaDescriptor>,
}

impl PluginMetadata {
    /// Validate the base manifest. Export/load must also validate extension
    /// factories before accepting a plugin with no processing components.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.abi_version == abi::ABI_VERSION && self.wire_version == abi::WIRE_VERSION,
            "unsupported native plugin ABI or wire format"
        );
        ComponentId::try_new(self.plugin.id.as_ref())?;
        ComponentId::try_new(self.plugin.version.as_ref())?;
        let mut schema_ids = BTreeSet::new();
        for schema in &self.schemas {
            anyhow::ensure!(
                schema_ids.insert((schema.id(), schema.version().value())),
                "duplicate native schema identity/version"
            );
        }
        let mut factories = BTreeSet::new();
        for factory in &self.factories {
            factory.validate(&self.plugin, &self.schemas)?;
            anyhow::ensure!(
                factories.insert(&factory.implementation),
                "duplicate native factory"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn optional_processing_capabilities_require_matching_contracts() -> anyhow::Result<()> {
        let schema = drasi_computation_contracts::GraphChangeCodec::schema();
        let plugin = PluginIdentity {
            id: "startup-continuation-test".into(),
            version: "1".into(),
        };
        let mut metadata = FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/recovered", "1")?,
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: ConfigSchema::from_fields([]),
            ports: vec![
                PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    schema.descriptor().clone(),
                    Default::default(),
                ),
                PortDescriptor::new(
                    PortId::try_new("out")?,
                    PortDirection::Output,
                    schema.descriptor().clone(),
                    Default::default(),
                ),
            ],
            completion: None,
            capabilities: Capabilities {
                startup_continuation: true,
                ..Default::default()
            },
        };
        metadata.implementation.plugin = Some(plugin.clone());
        let error = metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .expect_err("startup work requires declared continuation support");
        assert!(
            error.to_string().contains("startup continuation"),
            "{error:#}"
        );
        metadata.capabilities.continuations = true;
        metadata.validate(&plugin, &[schema.descriptor().clone()])?;
        metadata.capabilities.transactional = true;
        assert!(metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .is_err());
        metadata.capabilities.transactional = false;
        metadata.capabilities.committed_input_progress = Some(PortId::try_new("in")?);
        assert!(metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .is_err());
        metadata.capabilities.indexed_processing = true;
        metadata.validate(&plugin, &[schema.descriptor().clone()])?;
        metadata.capabilities.committed_input_progress = Some(PortId::try_new("missing")?);
        assert!(metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .is_err());
        metadata.capabilities.committed_input_progress = Some(PortId::try_new("in")?);
        metadata.capabilities.scheduled_processing = true;
        assert!(metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .is_err());
        metadata.capabilities.wakeups = true;
        metadata.validate(&plugin, &[schema.descriptor().clone()])?;
        metadata.capabilities.volatile_processing = true;
        assert!(metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .is_err());
        metadata.capabilities.volatile_processing = false;
        metadata.capabilities.non_atomic_publication = true;
        metadata.capabilities.durable_output = true;
        assert!(metadata
            .validate(&plugin, &[schema.descriptor().clone()])
            .is_err());
        Ok(())
    }

    #[test]
    fn field_schema_preserves_types_requiredness_and_closed_input() -> anyhow::Result<()> {
        let schema = ConfigSchema::from_fields([
            ("stream", ConfigType::String, true),
            ("count", ConfigType::Integer, false),
        ]);
        assert!(!schema.allow_additional);
        assert_eq!(schema.fields["stream"].value_type, ConfigType::String);
        assert!(schema.fields["stream"].required);
        assert!(!schema.fields["count"].required);
        assert!(schema.fields.values().all(|field| !field.secret));
        schema.validate(&json!({"stream": "events"}))?;
        schema.validate(&json!({"stream": "events", "count": 3}))?;
        assert!(schema.validate(&json!({})).is_err());
        assert!(schema.validate(&json!({"stream": 3})).is_err());
        assert!(schema
            .validate(&json!({"stream": "events", "extra": 1}))
            .is_err());
        assert!(!schema.allow_additional);
        Ok(())
    }
}
