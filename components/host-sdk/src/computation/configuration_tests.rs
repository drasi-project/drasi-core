// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

struct ConfigurationFactory(TestFactory);

impl sdk::Factory for ConfigurationFactory {
    fn metadata(&self) -> FactoryMetadata {
        sdk::Factory::metadata(&self.0)
    }
    fn configuration_details(&self) -> Option<sdk::ConfigDetails> {
        Some(
            sdk::ConfigDefinition::try_new([sdk::ConfigOption::new(
                "count",
                ConfigType::Integer,
                false,
                "Number of events.",
            )
            .default(2)
            .minimum(0)
            .maximum(4)])
            .expect("fixed declaration")
            .details(),
        )
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        control: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        sdk::Factory::create(&self.0, request, control)
    }
}

fn exported() -> &'static sdk::ExportedPlugin {
    static EXPORT: std::sync::OnceLock<sdk::ExportedPlugin> = std::sync::OnceLock::new();
    EXPORT.get_or_init(|| {
        sdk::ExportedPlugin::new(
            sdk::PluginDefinition::new(
                "test/configuration",
                "1",
                vec![Arc::new(ConfigurationFactory(TestFactory {
                    role: ComponentRole::Source,
                    name: "test/described-source",
                    controlled: false,
                    transactional: false,
                }))],
                vec![GraphChangeCodec::schema()],
            )
            .expect("definition"),
        )
        .expect("export")
    })
}

unsafe extern "C" fn metadata() -> *const abi::Metadata {
    exported().metadata()
}
unsafe extern "C" fn configuration() -> *const abi::configuration::ConfigurationMetadataV1 {
    exported().configuration()
}
unsafe extern "C" fn entry(out: *mut abi::PluginHandle) -> abi::Status {
    sdk::transport::status_boundary(|| unsafe { exported().entry(out) })
}
unsafe extern "C" fn incompatible_configuration(
) -> *const abi::configuration::ConfigurationMetadataV1 {
    Box::into_raw(Box::new(abi::configuration::ConfigurationMetadataV1 {
        header: abi::Header::new::<abi::configuration::ConfigurationMetadataV1>(),
        version: 999,
        reserved: 0,
        manifest: abi::BorrowedBytes::new(br#"{"version":1,"factories":[]}"#),
    }))
}

#[test]
fn configuration_extension_enriches_discovery_and_validates_before_construction(
) -> anyhow::Result<()> {
    let plugin = unsafe {
        NativePlugin::from_entry_points_with_configuration(
            metadata,
            entry,
            None,
            None,
            None,
            None,
            Some(configuration),
        )?
    };
    let factory = &plugin.factories()[0];
    assert!(factory.configuration_details().is_some());
    let schema = factory.configuration_schema();
    assert_eq!(schema["properties"]["count"]["default"], 2);
    assert_eq!(schema["properties"]["count"]["maximum"], 4);
    assert!(factory
        .specification(ComponentId::try_new("test")?, json!({"count":5}))
        .is_err());
    assert!(factory
        .create_component(ComponentId::try_new("test")?, json!({"count":5}))
        .is_err());
    factory.create_component(ComponentId::try_new("test")?, json!({"count":4}))?;
    Ok(())
}

#[test]
fn old_entry_points_keep_the_original_metadata_contract() -> anyhow::Result<()> {
    let plugin = unsafe { NativePlugin::from_entry_points(metadata, entry)? };
    let factory = &plugin.factories()[0];
    assert!(factory.configuration_details().is_none());
    assert!(factory.configuration_schema()["properties"]["count"]
        .get("maximum")
        .is_none());
    factory.specification(ComponentId::try_new("old-host")?, json!({"count":5}))?;
    // New plugins still validate their own description if an older host does not.
    assert!(factory
        .create_component(ComponentId::try_new("old-host")?, json!({"count":5}))
        .is_err());
    let base = serde_json::to_value(plugin.metadata())?;
    assert!(base["factories"][0]["configuration"]["fields"]["count"]
        .get("description")
        .is_none());
    Ok(())
}

#[test]
fn malformed_optional_configuration_is_not_ignored() {
    let result = unsafe {
        NativePlugin::from_entry_points_with_configuration(
            metadata,
            entry,
            None,
            None,
            None,
            None,
            Some(incompatible_configuration),
        )
    };
    let error = result.err().expect("invalid extension rejected");
    assert!(error
        .to_string()
        .contains("configuration-description extension"));
}
