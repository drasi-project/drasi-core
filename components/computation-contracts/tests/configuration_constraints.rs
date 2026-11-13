// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_contracts::{
    ComponentId, ConfigurationConstraint, OutputAliases, QueryExecutionSettings,
};
use serde_json::{json, Value};

const COMPONENT: &str = "00112233-4455-4677-8899-aabbccddeeff";

#[test]
fn output_alias_schema_matches_typed_bindings() {
    let constraint = ConfigurationConstraint::output_aliases();
    for value in [
        json!({}),
        json!({"orders": {"component": COMPONENT}}),
        json!({"orders": {"component": COMPONENT, "port": "out"}}),
        json!({"orders": {"component": COMPONENT, "port": null}}),
        json!({"orders": {"component": "source-name"}}),
        json!({"orders": {"component": {"id": COMPONENT}}}),
        json!({"orders": {"component": COMPONENT, "port": ""}}),
        json!({"orders": {"component": COMPONENT, "stream": "source/out"}}),
        json!({"invalid alias": {"component": COMPONENT}}),
        json!({"": {"component": COMPONENT}}),
    ] {
        let decoded = serde_json::from_value::<OutputAliases>(value.clone());
        let accepted = decoded.is_ok_and(|aliases| aliases.validate().is_ok());
        assert_eq!(constraint.accepts(&value), accepted, "{value}");
    }
}

#[test]
fn execution_inputs_have_no_name_selection_path() {
    let constraint = ConfigurationConstraint::query_execution();
    for inputs in [
        json!([]),
        json!([{"component": COMPONENT}]),
        json!([{"component": COMPONENT, "port": null}]),
        json!([{"component": COMPONENT, "nodes": ["Order"], "pipeline": ["normalize"]}]),
        json!(["source-name"]),
        json!([COMPONENT]),
        json!([{"id": COMPONENT}]),
        json!([{"component": {"id": COMPONENT}}]),
        json!([{"component": "source-name"}]),
    ] {
        let value = json!({"inputs": inputs});
        assert_eq!(
            constraint.accepts(&value),
            serde_json::from_value::<QueryExecutionSettings>(value.clone()).is_ok(),
            "{value}"
        );
    }
}

#[test]
fn snapshot_inputs_require_non_null_ports() {
    let constraint = ConfigurationConstraint::snapshot_inputs();
    assert!(constraint.accepts(&json!([{"component": COMPONENT, "port": "out"}])));
    for input in [
        json!({"component": COMPONENT}),
        json!({"component": COMPONENT, "port": null}),
        json!({"component": COMPONENT, "port": ""}),
        json!({"component": "source-name", "port": "out"}),
    ] {
        assert!(!constraint.accepts(&json!([input])));
    }
}

#[test]
fn assigned_queries_keep_explicit_authoring_reference_forms() {
    let constraint = ConfigurationConstraint::assigned_query_identity();
    for input in [
        json!("source-name"),
        json!(COMPONENT),
        json!({"id": COMPONENT}),
        json!({"component": "source-name", "port": "out"}),
        json!({"component": {"id": COMPONENT}, "port": null}),
    ] {
        assert!(constraint.accepts(&json!({
            "id": COMPONENT, "name": "orders", "inputs": [input],
        })));
    }
    for value in [
        json!({"id": COMPONENT}),
        json!({"name": "orders"}),
        json!({"id": null, "name": "orders"}),
        json!({"id": "orders", "name": "orders"}),
        json!({"id": COMPONENT, "name": "invalid name"}),
        json!({"id": COMPONENT, "name": "orders", "inputs": [{"id": "source-name"}]}),
    ] {
        assert!(!constraint.accepts(&value), "{value}");
    }
}

#[test]
fn constraints_round_trip_and_reject_schema_references() {
    let constraint = ConfigurationConstraint::output_aliases();
    let encoded = serde_json::to_value(&constraint).unwrap();
    assert_eq!(
        serde_json::from_value::<ConfigurationConstraint>(encoded).unwrap(),
        constraint
    );
    for schema in [
        json!({"$ref": "https://example.invalid/schema"}),
        json!({"$id": "file:///schema"}),
        json!({"properties": {"owner": {"$ref": "#/definitions/owner"}}}),
        json!({"items": [{"$ref": "#"}]}),
        json!({"dependencies": {"owner": {"$ref": "#"}}}),
        json!({"type": "not-a-json-type"}),
        Value::Null,
    ] {
        assert!(ConfigurationConstraint::try_new(schema).is_err());
    }
    let literals = ConfigurationConstraint::try_new(json!({
        "type": "object",
        "properties": {"$ref": {"type": "string"}},
        "default": {"$ref": "ordinary data", "$id": "ordinary data"},
        "const": {"$ref": "ordinary data"},
    }))
    .unwrap();
    assert!(literals.accepts(&json!({"$ref": "ordinary data"})));
}

#[test]
fn uuid_schema_and_canonical_decoder_agree() {
    let constraint =
        ConfigurationConstraint::try_new(json!({"type": "string", "format": "uuid"})).unwrap();
    for value in [
        COMPONENT,
        "00112233-4455-4677-8899-AABBCCDDEEFF",
        "00112233445546778899aabbccddeeff",
        "urn:uuid:00112233-4455-4677-8899-aabbccddeeff",
        "{00112233-4455-4677-8899-aabbccddeeff}",
        "source-name",
        "",
    ] {
        assert_eq!(
            constraint.accepts(&json!(value)),
            ComponentId::try_new(value).is_ok(),
            "{value}"
        );
    }
}
