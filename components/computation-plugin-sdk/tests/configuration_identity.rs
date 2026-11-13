// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_contracts::ConfigurationConstraint;
use drasi_computation_plugin_sdk::{
    configuration::{configuration_json_schema, ConfigDefinition, ConfigOption},
    ConfigSchema, ConfigType,
};
use serde_json::json;

const COMPONENT: &str = "00112233-4455-4677-8899-aabbccddeeff";

#[test]
fn nested_discovery_and_admission_use_the_same_constraint() {
    let mut schema = ConfigSchema::from_fields([
        ("owner", ConfigType::ComponentId, true),
        ("name", ConfigType::ComponentName, true),
        ("outputs", ConfigType::Object, true),
    ]);
    schema
        .constrain("outputs", ConfigurationConstraint::output_aliases())
        .unwrap();
    let discovered =
        ConfigurationConstraint::try_new(configuration_json_schema(&schema, None)).unwrap();
    for (owner, name, outputs, accepted) in [
        (
            COMPONENT,
            "orders",
            json!({"alias": {"component": COMPONENT}}),
            true,
        ),
        (COMPONENT, COMPONENT, json!({}), true),
        ("source-name", "orders", json!({}), false),
        (COMPONENT, "invalid name", json!({}), false),
        (
            COMPONENT,
            "orders",
            json!({"alias": {"component": "source-name"}}),
            false,
        ),
        (
            COMPONENT,
            "orders",
            json!({"invalid alias": {"component": COMPONENT}}),
            false,
        ),
    ] {
        let value = json!({"owner": owner, "name": name, "outputs": outputs});
        assert_eq!(schema.validate(&value).is_ok(), accepted, "{value}");
        assert_eq!(discovered.accepts(&value), accepted, "{value}");
    }
}

#[test]
fn configuration_defaults_must_satisfy_nested_identity_constraints() {
    for (component, accepted) in [(COMPONENT, true), ("source-name", false)] {
        let definition = ConfigDefinition::try_new([ConfigOption::new(
            "outputs",
            ConfigType::Object,
            false,
            "Named incoming outputs.",
        )
        .constraint(ConfigurationConstraint::output_aliases())
        .default(json!({"alias": {"component": component}}))]);
        assert_eq!(definition.is_ok(), accepted);
    }
}
