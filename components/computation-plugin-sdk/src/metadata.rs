// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Serializable discovery contracts. These are wire data, not ABI structs.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use drasi_lib::computation::v1::{
    ComponentDescriptor, ComponentId, ComponentRole, ComponentSemanticKind, ConfigurationField,
    ConfigurationSchema, ConfigurationType, FactoryDescriptor, ImplementationIdentity,
    PluginIdentity, PortDescriptor, PortDirection, SchemaDescriptor, SinkCompletion,
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
    fn runtime(self) -> ConfigurationType {
        match self {
            Self::Boolean => ConfigurationType::Boolean,
            Self::Integer => ConfigurationType::Integer,
            Self::String => ConfigurationType::String,
            Self::Object => ConfigurationType::Object,
            Self::Array => ConfigurationType::Array,
            Self::Json => ConfigurationType::Json,
        }
    }
    fn accepts(self, value: &Value) -> bool {
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
    pub fn runtime(&self) -> ConfigurationSchema {
        ConfigurationSchema {
            fields: self
                .fields
                .iter()
                .map(|(name, field)| {
                    (
                        Arc::from(name.as_str()),
                        ConfigurationField {
                            value_type: field.value_type.runtime(),
                            required: field.required,
                            secret: field.secret,
                        },
                    )
                })
                .collect(),
            allow_additional: self.allow_additional,
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
/// ignored. Source recovery, reconfiguration and dynamic query snapshot/outbox
/// interfaces are deliberately absent from ABI 1.0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub control: bool,
    pub readiness: bool,
    pub transactional: bool,
    pub continuations: bool,
    pub wakeups: bool,
    pub snapshot: bool,
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
        let descriptor = ComponentDescriptor::try_new(id, self.ports.clone())?;
        match &self.implementation.plugin {
            Some(plugin) => Ok(descriptor.with_plugin_identity(plugin.clone())?),
            None => Ok(descriptor),
        }
    }

    pub fn factory_descriptor(&self) -> FactoryDescriptor {
        FactoryDescriptor {
            implementation: self.implementation.clone(),
            role: self.role,
            configuration_version: self.configuration_version,
            configuration: self.configuration.runtime(),
            dependencies: BTreeMap::new(),
        }
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
        anyhow::ensure!(match self.role {
            ComponentRole::Source => inputs == 0 && outputs > 0,
            ComponentRole::Transformer => inputs > 0 && outputs > 0,
            ComponentRole::Sink => inputs > 0 && outputs == 0,
            ComponentRole::Service => inputs == 0 && outputs == 0,
            ComponentRole::Query => false,
        }, "unsupported native role or incompatible ports (dynamic query recovery is not implemented)");
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
                drasi_lib::computation::v1::validate_sink_completion(
                    completion,
                    port.requirements(),
                )?;
            }
        }
        let c = self.capabilities;
        anyhow::ensure!(
            !c.readiness || c.control,
            "native readiness requires the control interface"
        );
        anyhow::ensure!(
            !c.snapshot || self.role == ComponentRole::Sink,
            "snapshot capability requires a native sink"
        );
        anyhow::ensure!(
            !(c.continuations || c.wakeups || c.transactional)
                || self.role == ComponentRole::Transformer,
            "transformer capability on another role"
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
                || descriptor.semantic_kind() == Some(ComponentSemanticKind::from(self.role)),
            "native component claims unsupported semantic role"
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
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.abi_version == abi::ABI_VERSION && self.wire_version == abi::WIRE_VERSION,
            "unsupported native plugin ABI or wire format"
        );
        ComponentId::try_new(self.plugin.id.as_ref())?;
        ComponentId::try_new(self.plugin.version.as_ref())?;
        anyhow::ensure!(!self.factories.is_empty(), "native plugin has no factories");
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
