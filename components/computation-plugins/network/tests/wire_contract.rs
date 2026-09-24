// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use chrono::TimeZone;
use drasi_computation_network::{
    proto::{reaction, source},
    wire, GrpcSinkConfig, HttpSinkConfig, OutputFormat, SourceConfig,
};
use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementValue, SourceChange},
};
use drasi_lib::computation::v1::*;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

#[test]
fn http_wire_reuses_legacy_conversion_without_source_objects() -> Result<()> {
    for event in [
        json!({"operation":"insert","timestamp":1234567890000u64,"element":{
            "type":"node","id":"n","labels":["Room","Tracked"],"properties":{
                "integer":3,"float":3.25,"list":[1,true],"object":{"x":"y"}}}}),
        json!({"operation":"update","timestamp":9000000,"element":{
            "type":"relation","id":"r","labels":["LINKS"],"from":"left","to":"right","properties":{"value":7}}}),
        json!({"operation":"delete","id":"n","labels":["Room"],"timestamp":2000000}),
    ] {
        let dto: wire::HttpSourceChange = serde_json::from_value(event)?;
        let expected = drasi_source_http::convert_http_to_source_change(&dto, "facilities-db")?;
        assert_eq!(wire::http_change(dto, "facilities-db")?, expected);
    }
    let change = wire::http_change(
        serde_json::from_value(json!({
            "operation":"insert","element":{"type":"node","id":"n","labels":["Room"],"properties":{"i":3,"f":3.5}}
        }))?,
        "facilities-db",
    )?;
    let SourceChange::Insert {
        element: Element::Node {
            metadata,
            properties,
        },
    } = change
    else {
        unreachable!()
    };
    assert!(metadata.effective_from > 0);
    assert_eq!(properties.get("i"), Some(&ElementValue::Integer(3)));
    assert!(matches!(properties.get("f"), Some(ElementValue::Float(value)) if value.0 == 3.5));
    assert!(wire::http_change(
        serde_json::from_value(json!({
            "operation":"delete","id":""
        }))?,
        "facilities-db"
    )
    .is_err());
    Ok(())
}

fn reference(source_id: &str, element_id: &str) -> source::ElementReference {
    source::ElementReference {
        source_id: source_id.into(),
        element_id: element_id.into(),
    }
}
#[test]
fn grpc_wire_preserves_nanoseconds_labels_relation_direction_and_legacy_nested_types() -> Result<()>
{
    let metadata = source::ElementMetadata {
        reference: Some(reference("facilities-db", "relation")),
        labels: vec!["IN".into(), "REL".into()],
        effective_from: 1234567890,
    };
    let properties = drasi_reaction_grpc::helpers::convert_json_to_proto_struct(&json!({
        "integer":42,"float":2.25,"object":{"n":1},"list":[1,true],"null":null,"boolean":true
    }));
    let change = source::SourceChange {
        r#type: source::ChangeType::Insert as i32,
        source_id: "facilities-db".into(),
        timestamp: None,
        change: Some(source::source_change::Change::Element(source::Element {
            element: Some(source::element::Element::Relation(source::Relation {
                metadata: Some(metadata.clone()),
                in_node: Some(reference("left-source", "from")),
                out_node: Some(reference("right-source", "to")),
                properties: Some(properties),
            })),
        })),
    };
    let SourceChange::Insert {
        element:
            Element::Relation {
                metadata: actual,
                in_node,
                out_node,
                properties,
            },
    } = wire::grpc_change(change.clone(), "facilities-db")?
    else {
        unreachable!()
    };
    assert_eq!(actual.reference.source_id.as_ref(), "facilities-db");
    assert_eq!(actual.reference.element_id.as_ref(), "relation");
    assert_eq!(actual.effective_from, 1234);
    assert_eq!(
        actual.labels.as_ref(),
        &[Arc::<str>::from("IN"), Arc::<str>::from("REL")]
    );
    assert_eq!(in_node.source_id.as_ref(), "left-source");
    assert_eq!(in_node.element_id.as_ref(), "from");
    assert_eq!(out_node.source_id.as_ref(), "right-source");
    assert_eq!(out_node.element_id.as_ref(), "to");
    assert_eq!(properties.get("integer"), Some(&ElementValue::Integer(42)));
    assert_eq!(
        properties.get("float"),
        Some(&ElementValue::Float(2.25.into()))
    );
    assert_eq!(
        properties.get("object"),
        Some(&ElementValue::String(Arc::from("{\"n\":1.0}")))
    );
    assert_eq!(
        properties.get("list"),
        Some(&ElementValue::String(Arc::from("[1.0,true]")))
    );
    let delete = source::SourceChange {
        r#type: source::ChangeType::Delete as i32,
        source_id: "facilities-db".into(),
        timestamp: None,
        change: Some(source::source_change::Change::Metadata(metadata)),
    };
    assert!(
        matches!(wire::grpc_change(delete, "facilities-db")?, SourceChange::Delete { metadata } if metadata.effective_from == 1234)
    );
    let mut wrong = change;
    wrong.source_id = "wrong".into();
    assert!(wire::grpc_change(wrong, "facilities-db").is_err());
    Ok(())
}

fn input() -> Result<InputEnvelope> {
    let before: drasi_core::evaluation::context::QueryVariables =
        [("value".into(), VariableValue::Integer(2.into()))].into();
    let after: drasi_core::evaluation::context::QueryVariables = [
        ("value".into(), VariableValue::Integer(3.into())),
        ("float".into(), VariableValue::Float(3.75.into())),
        ("unsigned".into(), VariableValue::Integer(u64::MAX.into())),
        ("nonfinite".into(), VariableValue::Float(f64::NAN.into())),
        (
            "nested".into(),
            VariableValue::List(vec![VariableValue::Bool(true)]),
        ),
    ]
    .into();
    let results = vec![
        QueryPartEvaluationContext::Adding {
            after: after.clone(),
            row_signature: 11,
        },
        QueryPartEvaluationContext::Updating {
            before: before.clone(),
            after: after.clone(),
            row_signature: 12,
        },
        QueryPartEvaluationContext::Removing {
            before: before.clone(),
            row_signature: 13,
        },
        QueryPartEvaluationContext::Aggregation {
            before: None,
            after: after.clone(),
            grouping_keys: vec!["value".into()],
            default_before: true,
            default_after: false,
            row_signature: 14,
        },
        QueryPartEvaluationContext::Aggregation {
            before: Some(before),
            after,
            grouping_keys: vec!["value".into()],
            default_before: false,
            default_after: false,
            row_signature: 15,
        },
        QueryPartEvaluationContext::Noop,
    ];
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("q")?,
            SystemMetadata::new(StreamId::try_new("q/out")?, 42),
            &results,
            QueryOutputMetadata {
                query_id: "q".into(),
                source_id: Some("facilities-db".into()),
                timestamp: chrono::Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).unwrap(),
                metadata: HashMap::from([("origin".into(), json!({"value":1}))]),
                profiling: None,
            },
        )?
        .context("rows")?,
    })
}

#[test]
fn query_output_matches_legacy_http_dtos_and_grpc_canonical_fields() -> Result<()> {
    let input = input()?;
    let notifications = wire::notifications(&input, "q", &StreamId::try_new("q/out")?)?;
    assert_eq!(notifications.len(), 5, "Noop produces no outbound item");
    // Legacy projection is an oracle in this test only, never the native sink's execution path.
    let legacy = QueryChangeCodec::to_legacy_result(&input.envelope)?;
    let expected: Vec<_> = legacy
        .results
        .iter()
        .filter_map(|diff| {
            drasi_reaction_http::output::DefaultChangeNotification::from_diff(&legacy, diff)
        })
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    let actual: Vec<_> = notifications
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(actual, expected);
    assert_eq!(actual[3]["operation"], "UPDATE");
    assert!(actual[3].get("before").is_none());
    assert_eq!(actual[4]["before"], json!({"value":2}));
    for notification in &notifications {
        let item = wire::proto_item(notification, OutputFormat::CanonicalJson)?;
        assert_eq!(item.sequence, 42);
        assert_eq!(item.row_signature, notification.row_signature);
        assert_eq!(
            item.timestamp.as_ref().context("timestamp")?.seconds,
            legacy.timestamp.timestamp()
        );
        assert!(item.metadata.is_some());
        let expected_before = notification
            .before
            .as_ref()
            .map(drasi_reaction_grpc::helpers::convert_json_to_proto_struct);
        assert_eq!(item.before, expected_before);
        assert_eq!(
            item.after,
            notification
                .after
                .as_ref()
                .map(drasi_reaction_grpc::helpers::convert_json_to_proto_struct)
        );
        let mut expected_payload = serde_json::to_value(notification)?;
        expected_payload["rowSignature"] = notification.row_signature.into();
        assert_eq!(
            item.payload,
            Some(drasi_reaction_grpc::helpers::convert_json_to_proto_struct(
                &expected_payload
            ))
        );
        assert!(wire::proto_item(notification, OutputFormat::Proto)?
            .payload
            .is_none());
    }
    assert_eq!(
        wire::proto_item(&notifications[2], OutputFormat::Proto)?.item_type,
        reaction::QueryResultItemType::Delete as i32
    );
    assert!(wire::notifications(&input, "wrong", &StreamId::try_new("q/out")?).is_err());
    assert!(wire::notifications(&input, "q", &StreamId::try_new("wrong/out")?).is_err());
    Ok(())
}

#[test]
fn unsupported_profile_axes_and_invalid_headers_are_explicit_errors() -> Result<()> {
    assert_eq!(
        SourceConfig::parse(json!({"stream":"facilities-db/out"}), false)?.port,
        9000
    );
    assert_eq!(
        SourceConfig::parse(json!({"stream":"facilities-db/out"}), true)?.port,
        50051
    );
    for config in [
        json!({"stream":"s","adaptiveEnabled":true}),
        json!({"stream":"s","ingressCapacity":0}),
        json!({"stream":"s","durability":{}}),
        json!({"stream":"s","timeoutMs":0}),
    ] {
        assert!(SourceConfig::parse(config, false).is_err());
    }
    let http =
        HttpSinkConfig::parse(json!({"queryId":"q","url":"http://localhost:9001/reaction"}))?;
    assert_eq!(http.stream.as_str(), "q/out");
    assert_eq!(http.headers["x-query-sequence"], "q");
    for config in [
        json!({"queryId":"q","url":"http://localhost","outputTemplates":{}}),
        json!({"queryId":"q","url":"http://localhost","headers":{"x-query-sequence":"other"}}),
        json!({"queryId":"q","url":"http://localhost","headers":{"x-bad":"line\nbreak"}}),
    ] {
        assert!(HttpSinkConfig::parse(config).is_err());
    }
    let grpc = GrpcSinkConfig::parse(json!({"queryId":"q"}))?;
    assert_eq!(grpc.metadata["x-query-sequence"], "q");
    assert_eq!(grpc.max_retries, 5);
    for config in [
        json!({"queryId":"q","batchSize":2}),
        json!({"queryId":"q","batching":{"mode":"adaptive"}}),
        json!({"queryId":"q","metadata":{"x-query-sequence":"wrong"}}),
        json!({"queryId":"q","connectionRetryAttempts":0}),
    ] {
        assert!(GrpcSinkConfig::parse(config).is_err());
    }
    Ok(())
}
