// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::ConfigDefinition;
use drasi_core_types::models::{ElementMetadata, ElementPropertyMap, ElementReference};

fn declarations(value: Value) -> Vec<SourceMiddlewareConfig> {
    serde_json::from_value(value).expect("declaration shape")
}

fn pipeline(mode: Mode, middleware: Value, steps: &[&str]) -> Result<Pipeline> {
    Pipeline::new(
        mode,
        &declarations(middleware),
        &steps
            .iter()
            .map(|step| step.to_string())
            .collect::<Vec<_>>(),
    )
}

fn node(id: &str, label: &str, properties: Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: vec![Arc::from(label)].into(),
            effective_from: 1,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn labels(element: &Element) -> Vec<String> {
    element
        .get_metadata()
        .labels
        .iter()
        .map(ToString::to_string)
        .collect()
}

fn property(element: &Element, name: &str) -> Value {
    Value::from(&element.get_properties()[name])
}

/// A minimal valid declaration for every declared kind.
fn minimal(kind: &str) -> Value {
    let config = match kind {
        "decoder" => json!({"encoding_type": "base64", "target_property": "raw"}),
        "map" => json!({"Input": {"insert": [{"label": "Projected", "properties": {"v": "$.v"}}]}}),
        "parse_json" => json!({"target_property": "raw"}),
        "promote" => json!({"mappings": [{"path": "$.nested.v", "target_name": "v"}]}),
        "relabel" => json!({"labelMappings": {"Input": "Renamed"}}),
        other => panic!("no minimal declaration for {other}"),
    };
    json!({"name": "step", "kind": kind, "config": config})
}

fn definition(mode: Mode) -> ConfigDefinition {
    ConfigDefinition::try_new(mode.options()).expect("valid shared options")
}

#[test]
fn every_declared_kind_is_compiled_and_constructs_in_both_modes() -> Result<()> {
    let compiled: Vec<_> = stateless_middleware_factories()
        .iter()
        .map(|factory| factory.kind())
        .collect();
    for mode in [Mode::Events, Mode::Images] {
        for kind in mode.kinds() {
            assert!(
                compiled.contains(kind),
                "{kind} is declared but not compiled"
            );
            pipeline(mode, json!([minimal(kind)]), &["step"])
                .with_context(|| format!("{kind} in {mode:?}"))?;
        }
    }
    Ok(())
}

#[test]
fn the_published_schema_rejects_undeclared_kinds_and_malformed_declarations() {
    for mode in [Mode::Events, Mode::Images] {
        let schema = definition(mode).schema();
        let accepts = |middleware: Value| {
            schema
                .validate(&json!({"stream": "out", "middleware": middleware, "pipeline": []}))
                .is_ok()
        };
        for kind in mode.kinds() {
            assert!(accepts(json!([minimal(kind)])), "{kind} in {mode:?}");
        }
        for rejected in [
            json!([{"name": "x", "kind": "jq", "config": {}}]),
            json!([{"name": "x", "kind": "unwind", "config": {}}]),
            json!([{"name": "x", "kind": "unknown", "config": {}}]),
            json!([{"name": "x", "kind": "map"}]),
            json!([{"name": "x", "kind": "map", "config": {}, "extra": true}]),
            json!([{"name": "", "kind": "map", "config": {}}]),
            json!([{"name": "x", "kind": "map", "config": []}]),
        ] {
            assert!(!accepts(rejected.clone()), "{mode:?} accepted {rejected}");
        }
        assert!(schema
            .validate(&json!({"stream": "out", "middleware": [], "pipeline": [""]}))
            .is_err());
    }
}

#[test]
fn construction_rejects_what_binding_rejects() {
    for mode in [Mode::Events, Mode::Images] {
        let Err(error) = pipeline(
            mode,
            json!([{"name": "x", "kind": "jq", "config": {}}]),
            &["x"],
        ) else {
            panic!("jq is not declared");
        };
        assert!(error.to_string().contains("does not declare"), "{error:#}");
        // Both declarations are named `step`.
        assert!(pipeline(mode, json!([minimal("map"), minimal("relabel")]), &[]).is_err());
        assert!(pipeline(mode, json!([minimal("map")]), &["missing"]).is_err());
        assert!(pipeline(
            mode,
            json!([{"name": "x", "kind": "decoder", "config": {"target_property": "raw"}}]),
            &["x"]
        )
        .is_err());
    }
}

#[test]
fn images_mode_declares_and_enforces_insert_only_map_rules() {
    let update_rules =
        json!([{"name": "m", "kind": "map", "config": {"Input": {"insert": [], "update": []}}}]);
    let delete_output = json!([{"name": "m", "kind": "map",
        "config": {"Input": {"insert": [{"op": "Delete"}]}}}]);
    let update_output = json!([{"name": "m", "kind": "map",
        "config": {"Input": {"insert": [{"op": "Update"}]}}}]);
    let replace_output = json!([{"name": "m", "kind": "map",
        "config": {"Input": {"insert": [{"op": "Replace", "label": "P"}]}}}]);
    let images = definition(Mode::Images).schema();
    let events = definition(Mode::Events).schema();
    for middleware in [&update_rules, &delete_output, &update_output] {
        let configuration = json!({"stream": "out", "middleware": middleware, "pipeline": ["m"]});
        assert!(images.validate(&configuration).is_err(), "{middleware}");
        assert!(events.validate(&configuration).is_ok(), "{middleware}");
        assert!(pipeline(Mode::Images, middleware.clone(), &["m"]).is_err());
        assert!(pipeline(Mode::Events, middleware.clone(), &["m"]).is_ok());
    }
    assert!(pipeline(Mode::Images, replace_output, &["m"]).is_ok());
}

#[tokio::test]
async fn event_pipelines_fan_out_and_apply_later_steps_to_every_output() -> Result<()> {
    let pipeline = pipeline(
        Mode::Events,
        json!([
            {"name": "split", "kind": "map", "config": {"Input": {"insert": [
                {"label": "A", "id": "$.a", "properties": {"v": "$.a"}},
                {"label": "B", "id": "$.b", "properties": {"v": "$.b"}},
            ]}}},
            {"name": "rename", "kind": "relabel", "config": {"labelMappings": {"A": "Renamed"}}},
        ]),
        &["split", "rename"],
    )?;
    let output = pipeline
        .apply(SourceChange::Insert {
            element: node("1", "Input", json!({"a": "x", "b": "y"})),
        })
        .await?;
    let output: Vec<_> = output
        .into_iter()
        .map(|change| match change {
            SourceChange::Insert { element } => (labels(&element), property(&element, "v")),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        output,
        vec![
            (vec!["Renamed".to_owned()], json!("x")),
            (vec!["B".to_owned()], json!("y")),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn stateless_kinds_previously_missing_from_the_native_path_now_run() -> Result<()> {
    let pipeline = pipeline(
        Mode::Events,
        json!([
            {"name": "decode", "kind": "decoder",
             "config": {"encoding_type": "base64", "target_property": "raw"}},
            {"name": "parse", "kind": "parse_json", "config": {"target_property": "raw"}},
            {"name": "lift", "kind": "promote",
             "config": {"mappings": [{"path": "$.raw.v", "target_name": "v"}]}},
        ]),
        &["decode", "parse", "lift"],
    )?;
    // base64 of {"v":7}
    let output = pipeline
        .apply(SourceChange::Insert {
            element: node("1", "Input", json!({"raw": "eyJ2Ijo3fQ=="})),
        })
        .await?;
    let [SourceChange::Insert { element }] = output.as_slice() else {
        panic!("expected one insert, got {output:?}");
    };
    assert_eq!(property(element, "v"), json!(7));
    Ok(())
}

#[tokio::test]
async fn image_pipelines_return_complete_images_and_an_empty_pipeline_is_identity() -> Result<()> {
    let identity = pipeline(Mode::Images, json!([]), &[])?;
    let input = node("1", "Input", json!({"v": 1}));
    assert_eq!(identity.map(input.clone()).await?, vec![input.clone()]);

    let projected = pipeline(
        Mode::Images,
        json!([
            {"name": "project", "kind": "map",
             "config": {"Input": {"insert": [{"label": "Projected", "properties": {"w": "$.v"}}]}}},
            {"name": "rename", "kind": "relabel",
             "config": {"labelMappings": {"Projected": "View"}}},
        ]),
        &["project", "rename"],
    )?;
    let output = projected.map(input).await?;
    assert_eq!(output.len(), 1);
    assert_eq!(labels(&output[0]), vec!["View".to_owned()]);
    assert_eq!(property(&output[0], "w"), json!(1));
    Ok(())
}

#[test]
fn other_registries_publish_their_own_kinds_with_the_same_declaration_shape() -> Result<()> {
    let constraint = ConfigurationConstraint::middleware_declarations(&["custom", "unwind"])?;
    assert!(constraint.accepts(&json!([{"name": "x", "kind": "unwind", "config": {}}])));
    assert!(constraint.accepts(&json!([{"name": "x", "kind": "custom", "config": {}}])));
    assert!(!constraint.accepts(&json!([{"name": "x", "kind": "map", "config": {}}])));
    assert!(!constraint.accepts(&json!([{"name": "x", "kind": "custom"}])));
    Ok(())
}
