// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Per-port format overrides for native factories.
//!
//! A factory's `ports` are its default descriptors. Its
//! `capabilities.port_formats` lists the extra schemas it genuinely implements
//! (each must be declared and validated in the plugin's `schemas`). A
//! configuration may select one supported format per port through the
//! reserved `ports` field:
//!
//! ```json
//! { "ports": { "out": { "format": "vendor.columnar-batch-v1" } } }
//! ```
//!
//! Selection is pure: it only chooses among declared schemas and rejects
//! unknown ports or formats rather than defaulting or converting.
//!
//! A factory opts into the reserved `ports` field by declaring alternatives or
//! by declaring `ports` as an optional object configuration field
//! ([`ConfigSchema::with_port_formats`]). Once opted in, an invalid selection is
//! always an error; with no alternatives only each port's own format is
//! accepted. A factory that does neither keeps `ports` as an ordinary
//! plugin-defined field.

use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, PortDirection, PortFormatCatalogue, PortId, PortPrimaries,
    SchemaDescriptor, PORT_FORMATS_KEY,
};
use serde_json::Value;

use crate::metadata::{ConfigField, ConfigSchema, ConfigType, FactoryMetadata};

impl ConfigSchema {
    /// Declare the optional `ports` object that opts a factory into format
    /// selection (required when it declares alternatives).
    pub fn with_port_formats(mut self) -> Self {
        self.fields.insert(
            PORT_FORMATS_KEY.to_owned(),
            ConfigField {
                value_type: ConfigType::Object,
                required: false,
                secret: false,
            },
        );
        self
    }
}

impl FactoryMetadata {
    /// Whether the reserved `ports` field selects port formats: the factory
    /// declares alternatives or an optional non-secret object `ports` field.
    pub fn supports_port_format_overrides(&self) -> bool {
        !self.capabilities.port_formats.is_empty()
            || self
                .configuration
                .fields
                .get(PORT_FORMATS_KEY)
                .is_some_and(|field| {
                    field.value_type == ConfigType::Object && !field.required && !field.secret
                })
    }

    /// The default ports and every supported alternative. Call on factory
    /// metadata, never on the output of [`Self::for_configuration`].
    pub fn port_catalogue(&self) -> anyhow::Result<PortFormatCatalogue> {
        Ok(PortFormatCatalogue::try_new(
            self.ports.clone(),
            self.capabilities.port_formats.clone(),
        )?)
    }

    /// Factory metadata whose `ports` are those the configuration's `ports`
    /// field selects. Capabilities are unchanged. A factory that has not opted
    /// into format selection is returned as is and its `ports` field is
    /// ignored here.
    pub fn for_configuration(&self, configuration: &Value) -> anyhow::Result<Self> {
        self.for_port_overrides(configuration.get(PORT_FORMATS_KEY))
    }

    /// Like [`Self::for_configuration`], from the `ports` value itself.
    pub fn for_port_overrides(&self, overrides: Option<&Value>) -> anyhow::Result<Self> {
        let mut resolved = self.clone();
        if self.supports_port_format_overrides() {
            resolved.ports = self.port_catalogue()?.resolve(overrides)?;
        }
        Ok(resolved)
    }

    /// Component descriptor for a configuration, with its selected formats.
    pub fn descriptor_for(
        &self,
        id: ComponentId,
        configuration: &Value,
    ) -> anyhow::Result<ComponentDescriptor> {
        self.for_configuration(configuration)?.descriptor(id)
    }

    /// The designated primary input and output ports.
    pub fn primary_ports(&self) -> &PortPrimaries {
        &self.capabilities.primary_ports
    }

    /// Resolve a shorthand endpoint to one of this factory's ports (see
    /// [`PortPrimaries::resolve`]). Port ids do not change with format
    /// selection, so the factory's default ports answer for every configuration.
    pub fn resolve_endpoint_port(
        &self,
        direction: PortDirection,
        explicit: Option<&PortId>,
    ) -> anyhow::Result<PortId> {
        Ok(self
            .capabilities
            .primary_ports
            .resolve(&self.ports, direction, explicit)?)
    }

    /// Manifest checks: valid primary designations, a consistent catalogue,
    /// every alternative declared in the plugin's executable `schemas`, and an
    /// optional object `ports` configuration field to carry the selection.
    pub(crate) fn validate_port_formats(&self, schemas: &[SchemaDescriptor]) -> anyhow::Result<()> {
        self.capabilities.primary_ports.validate(&self.ports)?;
        self.port_catalogue()?;
        if !self.supports_port_format_overrides() {
            return Ok(());
        }
        for alternative in &self.capabilities.port_formats {
            anyhow::ensure!(
                schemas.iter().any(|schema| schema == &alternative.schema),
                "native port {} format {} uses an undeclared or conflicting schema",
                alternative.port,
                drasi_computation_contracts::port_format_name(&alternative.schema)
            );
        }
        anyhow::ensure!(
            self.configuration
                .fields
                .get(PORT_FORMATS_KEY)
                .is_some_and(|field| field.value_type == ConfigType::Object
                    && !field.required
                    && !field.secret),
            "native factory with port format alternatives must declare an optional object `ports` configuration field"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use drasi_computation_contracts::{
        ComponentRole, ImplementationIdentity, PipeRequirements, PortDescriptor, PortDirection,
        PortFormatAlternative, PortId, SchemaId, SchemaVersion,
    };
    use serde_json::json;

    use super::*;
    use crate::metadata::Capabilities;

    fn schema(id: &str) -> SchemaDescriptor {
        SchemaDescriptor::try_new(
            SchemaId::try_new(id).unwrap(),
            SchemaVersion::try_new(1).unwrap(),
            "test-encoding",
            Bytes::from_static(b"definition"),
        )
        .unwrap()
    }

    fn port(id: &str) -> PortId {
        PortId::try_new(id).unwrap()
    }

    fn metadata(alternatives: Vec<PortFormatAlternative>) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/ports", "1").unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: ConfigSchema::default().with_port_formats(),
            ports: vec![
                PortDescriptor::new(
                    port("in"),
                    PortDirection::Input,
                    schema("graph"),
                    PipeRequirements::default(),
                ),
                PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    schema("graph"),
                    PipeRequirements::default(),
                ),
            ],
            completion: None,
            capabilities: Capabilities {
                port_formats: alternatives,
                ..Capabilities::default()
            },
        }
    }

    fn alternative(port_id: &str, schema_id: &str) -> PortFormatAlternative {
        PortFormatAlternative {
            port: port(port_id),
            schema: schema(schema_id),
        }
    }

    #[test]
    fn default_and_selected_ports_resolve_from_configuration() {
        let metadata = metadata(vec![alternative("out", "vendor.batch")]);
        let default = metadata.descriptor_for(ComponentId::try_new("c").unwrap(), &json!({}));
        assert_eq!(default.unwrap().ports(), metadata.ports);
        let selected = metadata
            .descriptor_for(
                ComponentId::try_new("c").unwrap(),
                &json!({"ports": {"out": {"format": "vendor.batch-v1"}}}),
            )
            .unwrap();
        assert_eq!(selected.ports()[0], metadata.ports[0]);
        assert_eq!(selected.ports()[1].schema(), &schema("vendor.batch"));
    }

    #[test]
    fn unsupported_selection_is_rejected_not_defaulted() {
        let metadata = metadata(vec![alternative("out", "vendor.batch")]);
        for ports in [
            json!({"out": {"format": "vendor.other-v1"}}),
            json!({"in": {"format": "vendor.batch-v1"}}),
            json!({"missing": {"format": "graph-v1"}}),
        ] {
            assert!(
                metadata
                    .for_configuration(&json!({ "ports": ports }))
                    .is_err(),
                "{ports}"
            );
        }
    }

    #[test]
    fn declared_ports_field_without_alternatives_resolves_a_fixed_catalogue() {
        let metadata = metadata(Vec::new());
        assert!(metadata.supports_port_format_overrides());
        assert_eq!(
            metadata.for_configuration(&json!({})).unwrap().ports,
            metadata.ports
        );
        assert_eq!(
            metadata
                .for_configuration(&json!({"ports": {"out": {"format": "graph-v1"}}}))
                .unwrap()
                .ports,
            metadata.ports
        );
        for ports in [
            json!([8080]),
            json!({"out": {}}),
            json!({"out": {"format": "vendor.batch-v1"}}),
            json!({"missing": {"format": "graph-v1"}}),
        ] {
            assert!(
                metadata
                    .for_configuration(&json!({ "ports": ports }))
                    .is_err(),
                "{ports}"
            );
        }
        metadata.validate_port_formats(&[schema("graph")]).unwrap();
    }

    #[test]
    fn unrelated_ports_field_is_untouched_without_opt_in() {
        let mut metadata = metadata(Vec::new());
        for field in [
            None,
            Some(ConfigField {
                value_type: ConfigType::Array,
                required: false,
                secret: false,
            }),
            Some(ConfigField {
                value_type: ConfigType::Object,
                required: true,
                secret: false,
            }),
        ] {
            metadata.configuration = ConfigSchema::default();
            if let Some(field) = field {
                metadata
                    .configuration
                    .fields
                    .insert(PORT_FORMATS_KEY.to_owned(), field);
            }
            assert!(!metadata.supports_port_format_overrides());
            let resolved = metadata
                .for_configuration(&json!({"ports": [8080]}))
                .unwrap();
            assert_eq!(resolved.ports, metadata.ports);
        }
    }

    #[test]
    fn manifest_requires_declared_schemas_and_ports_field() {
        let mut metadata = metadata(vec![alternative("out", "vendor.batch")]);
        let declared = [schema("graph"), schema("vendor.batch")];
        metadata.validate_port_formats(&declared).unwrap();
        assert!(metadata.validate_port_formats(&declared[..1]).is_err());
        metadata.configuration = ConfigSchema::default();
        assert!(metadata.validate_port_formats(&declared).is_err());
    }

    #[test]
    fn primary_ports_are_validated_with_the_manifest_and_survive_selection() {
        use drasi_computation_contracts::PluginIdentity;

        let plugin = PluginIdentity {
            id: "plugin".into(),
            version: "1".into(),
        };
        let schemas = [schema("graph"), schema("vendor.batch")];
        let mut metadata = metadata(vec![alternative("out", "vendor.batch")]);
        metadata.implementation.plugin = Some(plugin.clone());
        metadata.ports.push(PortDescriptor::new(
            port("audit"),
            PortDirection::Output,
            schema("graph"),
            PipeRequirements::default(),
        ));
        metadata.validate(&plugin, &schemas).unwrap();

        // Two outputs and no designation are ambiguous; the single input is not.
        assert_eq!(
            metadata
                .resolve_endpoint_port(PortDirection::Input, None)
                .unwrap(),
            port("in")
        );
        assert!(metadata
            .resolve_endpoint_port(PortDirection::Output, None)
            .is_err());

        for invalid in [
            PortPrimaries::new(None, Some(port("missing"))),
            PortPrimaries::new(None, Some(port("in"))),
            PortPrimaries::new(Some(port("out")), None),
        ] {
            metadata.capabilities.primary_ports = invalid;
            assert!(metadata.validate(&plugin, &schemas).is_err());
            assert!(metadata
                .resolve_endpoint_port(PortDirection::Output, None)
                .is_err());
        }

        metadata.capabilities.primary_ports = PortPrimaries::new(None, Some(port("out")));
        metadata.validate(&plugin, &schemas).unwrap();
        assert_eq!(
            metadata
                .resolve_endpoint_port(PortDirection::Output, None)
                .unwrap(),
            port("out")
        );
        let selected = metadata
            .for_configuration(&json!({"ports": {"out": {"format": "vendor.batch-v1"}}}))
            .unwrap();
        assert_eq!(
            selected
                .resolve_endpoint_port(PortDirection::Output, None)
                .unwrap(),
            port("out")
        );

        // The manifest carries the designation only when one exists.
        let encoded = serde_json::to_value(&metadata.capabilities).unwrap();
        assert_eq!(encoded["primary_ports"], json!({"output": "out"}));
        metadata.capabilities.primary_ports = PortPrimaries::default();
        let encoded = serde_json::to_value(&metadata.capabilities).unwrap();
        assert!(encoded.get("primary_ports").is_none());
    }
}
