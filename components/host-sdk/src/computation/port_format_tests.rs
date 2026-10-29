// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Per-port format overrides across the exported native boundary: discovery,
//! specification, validation and construction all follow the same resolution,
//! and an unsupported selection fails before a component is constructed.

use super::*;

const ALTERNATIVE_FORMAT: &str = "test.alt-graph-v1";

fn alternative_descriptor() -> SchemaDescriptor {
    SchemaDescriptor::try_new(
        SchemaId::try_new("test.alt-graph").unwrap(),
        SchemaVersion::try_new(1).unwrap(),
        "test-encoding",
        bytes::Bytes::from_static(b"alternative-definition"),
    )
    .unwrap()
}

/// This test never produces records in the alternative format.
struct NoRecords;

impl RecordValidator for NoRecords {
    fn validate_identity(
        &self,
        _: &SchemaDescriptor,
        _: &RecordId,
    ) -> std::result::Result<(), RecordValidationError> {
        Err(RecordValidationError::new("test", "no records expected"))
    }

    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        _: RecordImage,
        _: &[u8],
    ) -> std::result::Result<(), RecordValidationError> {
        self.validate_identity(schema, identity)
    }
}

struct FormatFactory;

impl sdk::Factory for FormatFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = sdk::Factory::metadata(&TestFactory {
            role: ComponentRole::Source,
            name: "test/port-formats",
            controlled: false,
            transactional: false,
        });
        metadata.configuration = metadata.configuration.with_port_formats();
        metadata.capabilities.port_formats = vec![sdk::PortFormatAlternative {
            port: PortId::try_new("out").unwrap(),
            schema: alternative_descriptor(),
        }];
        metadata.capabilities.primary_ports =
            PortPrimaries::new(None, Some(PortId::try_new("out").unwrap()));
        metadata
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let metadata = sdk::Factory::metadata(self).for_configuration(&request.configuration)?;
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Source(Box::new(TestComponent {
                descriptor: metadata.descriptor(request.id.clone())?,
                configuration: request.configuration.clone(),
                seen: 0,
                notification: Arc::new(tokio::sync::Notify::new()),
                controlled: false,
            })),
            control_handler: None,
        })
    }
}

fn exported() -> &'static sdk::ExportedPlugin {
    static EXPORT: std::sync::OnceLock<sdk::ExportedPlugin> = std::sync::OnceLock::new();
    EXPORT.get_or_init(|| {
        sdk::ExportedPlugin::new(
            sdk::PluginDefinition::new(
                "test/port-formats",
                "1",
                vec![Arc::new(FormatFactory)],
                vec![
                    GraphChangeCodec::schema(),
                    Arc::new(Schema::new(alternative_descriptor(), Arc::new(NoRecords))),
                ],
            )
            .expect("definition"),
        )
        .expect("export")
    })
}

unsafe extern "C" fn metadata() -> *const abi::Metadata {
    exported().metadata()
}
unsafe extern "C" fn entry(out: *mut abi::PluginHandle) -> abi::Status {
    sdk::transport::status_boundary(|| unsafe { exported().entry(out) })
}

#[test]
fn selected_format_is_described_validated_and_constructed() -> anyhow::Result<()> {
    let plugin = unsafe { NativePlugin::from_entry_points(metadata, entry)? };
    let factory = &plugin.factories()[0];
    let id = ComponentId::try_new("test")?;
    let default_schema = GraphChangeCodec::schema().descriptor().clone();

    let default = factory.specification(id.clone(), json!({}))?;
    assert_eq!(default.descriptor.ports()[0].schema(), &default_schema);
    factory.validate(&default)?;
    assert!(factory.port_formats().is_some());
    assert_eq!(
        factory.primary_ports(),
        PortPrimaries::new(None, Some(PortId::try_new("out")?))
    );
    assert_eq!(
        factory
            .primary_ports()
            .resolve(default.descriptor.ports(), PortDirection::Output, None)?,
        PortId::try_new("out")?
    );

    let overrides = json!({"ports": {"out": {"format": ALTERNATIVE_FORMAT}}});
    let selected = factory.specification(id.clone(), overrides.clone())?;
    assert_eq!(
        selected.descriptor.ports()[0].schema(),
        &alternative_descriptor()
    );
    factory.validate(&selected)?;
    let created = factory.create_component(id.clone(), overrides)?;
    assert_eq!(
        created.descriptor().ports()[0].schema(),
        &alternative_descriptor()
    );

    // A specification whose ports do not follow its configuration is rejected.
    let mut forged = default;
    forged.configuration.insert(
        Arc::from("ports"),
        ConfigurationValue::Literal(json!({"out": {"format": ALTERNATIVE_FORMAT}})),
    );
    assert!(factory.validate(&forged).is_err());
    Ok(())
}

#[test]
fn unsupported_selection_is_rejected_before_construction() -> anyhow::Result<()> {
    let plugin = unsafe { NativePlugin::from_entry_points(metadata, entry)? };
    let factory = &plugin.factories()[0];
    let id = ComponentId::try_new("test")?;
    for ports in [
        json!({"out": {"format": "test.unknown-v1"}}),
        json!({"missing": {"format": ALTERNATIVE_FORMAT}}),
        json!({"out": {}}),
        json!("out"),
    ] {
        let configuration = json!({ "ports": ports });
        assert!(
            factory
                .specification(id.clone(), configuration.clone())
                .is_err(),
            "{configuration}"
        );
        assert!(
            factory.create_component(id.clone(), configuration).is_err(),
            "{ports}"
        );
    }
    Ok(())
}

/// A manifest whose primary designation names a port that does not exist.
struct InvalidPrimary;

impl sdk::Factory for InvalidPrimary {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = sdk::Factory::metadata(&FormatFactory);
        metadata.capabilities.primary_ports =
            PortPrimaries::new(None, Some(PortId::try_new("missing").unwrap()));
        metadata
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        control: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        sdk::Factory::create(&FormatFactory, request, control)
    }
}

#[test]
fn invalid_primary_designation_is_rejected_at_export() {
    let exported = sdk::PluginDefinition::new(
        "test/invalid-primary",
        "1",
        vec![Arc::new(InvalidPrimary)],
        vec![
            GraphChangeCodec::schema(),
            Arc::new(Schema::new(alternative_descriptor(), Arc::new(NoRecords))),
        ],
    )
    .and_then(sdk::ExportedPlugin::new);
    assert!(exported.is_err());
}
