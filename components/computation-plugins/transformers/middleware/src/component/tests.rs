// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::ComponentId;
use drasi_computation_plugin_sdk::{
    lifecycle::{CancellationToken, OperationScope},
    Factory,
};
use drasi_core_types::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};
use serde_json::{json, Value};

const ID: &str = "0b5f0d55-1d6f-4f6c-9d7c-4b9cbd0bb0a1";

fn scope() -> OperationScope {
    OperationScope::new("test", "component", 1, None, CancellationToken::new())
}

fn create(configuration: Value) -> Result<Middleware> {
    Middleware::new(
        crate::MiddlewareFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        configuration,
        None,
    )
}

async fn start(configuration: Value) -> Result<Middleware> {
    let mut middleware = create(configuration)?;
    middleware
        .start(&mut StartContext::new(scope(), None))
        .await?;
    Ok(middleware)
}

fn row(id: &str, properties: Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: vec![Arc::from("Input")].into(),
                effective_from: 1,
            },
            properties: ElementPropertyMap::from(properties),
        },
    }
}

fn input(changes: &[SourceChange], sequence: u64) -> Result<InputEnvelope> {
    Ok(InputEnvelope { execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

fn decoded(outputs: &[OutputEnvelope]) -> Result<Vec<SourceChange>> {
    let [output] = outputs else {
        anyhow::bail!("expected one output envelope");
    };
    Ok(GraphChangeCodec::decode_changes(&output.envelope)?)
}

fn decode_raw() -> Value {
    json!({
        "stream": "middleware/out",
        "middleware": [{"name": "decode", "kind": "decoder",
                        "config": {"encoding_type": "base64", "target_property": "raw"}}],
        "pipeline": ["decode"]
    })
}

#[test]
fn the_factory_declares_its_kinds_so_binding_rejects_undeclared_ones() {
    let schema = crate::MiddlewareFactory.metadata().configuration;
    let configuration = |kind: &str| {
        json!({"stream": "out", "middleware": [{"name": "x", "kind": kind, "config": {}}],
               "pipeline": ["x"]})
    };
    for kind in drasi_computation_middleware_common::STATELESS_KINDS {
        assert!(schema.validate(&configuration(kind)).is_ok(), "{kind}");
    }
    for kind in ["jq", "unwind", "custom"] {
        assert!(schema.validate(&configuration(kind)).is_err(), "{kind}");
    }
    let details = crate::MiddlewareFactory.configuration_details();
    assert!(details.fields["middleware"]
        .description
        .contains("decoder, map, parse_json, promote, relabel"));
}

#[test]
fn construction_rejects_duplicate_and_undeclared_pipeline_names() {
    let mut duplicate = decode_raw();
    let repeated = duplicate["middleware"][0].clone();
    duplicate["middleware"]
        .as_array_mut()
        .unwrap()
        .push(repeated);
    let mut undeclared = decode_raw();
    undeclared["pipeline"] = json!(["decode", "missing"]);
    for invalid in [duplicate, undeclared] {
        assert!(create(invalid.clone()).is_err(), "accepted {invalid}");
    }
}

#[tokio::test]
async fn every_change_in_an_envelope_runs_through_the_pipeline_in_order() -> Result<()> {
    let mut middleware = start(json!({
        "stream": "middleware/out",
        "middleware": [{"name": "split", "kind": "map", "config": {"Input": {"insert": [
            {"label": "A", "id": "$.a", "properties": {"v": "$.a"}},
            {"label": "B", "id": "$.b", "properties": {"v": "$.b"}},
        ]}}}],
        "pipeline": ["split"]
    }))
    .await?;
    let outputs = middleware
        .transform(input(
            &[
                row("1", json!({"a": "1a", "b": "1b"})),
                row("2", json!({"a": "2a", "b": "2b"})),
            ],
            1,
        )?)
        .await?;
    let ids: Vec<_> = decoded(&outputs)?
        .iter()
        .map(|change| match change {
            SourceChange::Insert { element } => element.get_reference().element_id.to_string(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(ids, ["1a", "1b", "2a", "2b"]);
    Ok(())
}

#[tokio::test]
async fn a_failed_step_latches_until_the_component_is_reconstructed() -> Result<()> {
    let mut middleware = start(decode_raw()).await?;
    assert!(middleware
        .transform(input(&[row("1", json!({"raw": "not base64!"}))], 1)?)
        .await
        .is_err());
    let valid = || input(&[row("2", json!({"raw": "aGk="}))], 2);
    let Err(error) = middleware.transform(valid()?).await else {
        anyhow::bail!("a latched middleware accepted input");
    };
    assert!(format!("{error:#}").contains("reconstruction"), "{error:#}");
    middleware
        .stop(&StopContext::new(
            scope(),
            drasi_computation_plugin_sdk::lifecycle::StopMode::Drain,
        ))
        .await?;
    assert!(middleware
        .start(&mut StartContext::new(scope(), None))
        .await
        .is_err());

    let mut rebuilt = start(decode_raw()).await?;
    let changes = decoded(&rebuilt.transform(valid()?).await?)?;
    let [SourceChange::Insert { element }] = changes.as_slice() else {
        anyhow::bail!("expected one insert, got {changes:?}");
    };
    assert_eq!(Value::from(&element.get_properties()["raw"]), json!("hi"));
    Ok(())
}
