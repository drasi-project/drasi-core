// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use chrono::TimeZone;
use drasi_computation_contracts::{SinkOutput, SinkOutputs};
use drasi_computation_network_common::{proto::source, wire};
use drasi_computation_sink_grpc::GrpcSinkConfig;
use drasi_computation_sink_http::HttpSinkConfig;
use drasi_computation_source_grpc::GrpcSourceConfig;
use drasi_computation_source_http::HttpSourceConfig;
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
            &ResultNodeProjector::try_new("test/native-network-wire/q/out", None)?,
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

/// The same records and system metadata with no query annotation at all.
fn unannotated(input: &InputEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: input.port.clone(),
        envelope: ChangeEnvelope::new(
            input.envelope.id().clone(),
            input.envelope.changes().clone(),
            (**input.envelope.system()).clone(),
        ),
    }
}

fn source_input(changes: &[SourceChange]) -> Result<InputEnvelope> {
    source_input_on("facilities-db/out", changes)
}

fn source_input_on(stream: &str, changes: &[SourceChange]) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new(stream)?,
            7,
            chrono::Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).single(),
        )?,
    })
}

fn source_input_untimed(changes: &[SourceChange]) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new("facilities-db/out")?,
            7,
            None,
        )?,
    })
}

fn room(id: &str, name: &str) -> Element {
    let mut properties = drasi_core::models::ElementPropertyMap::new();
    properties.insert("name", ElementValue::String(name.into()));
    Element::Node {
        metadata: drasi_core::models::ElementMetadata {
            reference: drasi_core::models::ElementReference::new("facilities-db", id),
            labels: Arc::from([Arc::from("Room")]),
            effective_from: 1_000,
        },
        properties,
    }
}

fn outputs(bindings: &[(&str, &str, &str, &str)]) -> Result<SinkOutputs> {
    Ok(SinkOutputs::new(
        bindings
            .iter()
            .map(|(alias, component, port, stream)| {
                Ok((
                    (*alias).to_owned(),
                    SinkOutput {
                        component: ComponentId::try_new(*component)?,
                        port: PortId::try_new(*port)?,
                        stream: StreamId::try_new(*stream)?,
                    },
                ))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?,
    )?)
}

fn bodies(input: &InputEnvelope, bindings: &SinkOutputs) -> Result<Vec<Value>> {
    wire::notifications(input, bindings)?
        .iter()
        .map(|notification| Ok(notification.to_json()?))
        .collect()
}

#[test]
fn query_output_is_ordinary_graph_data_without_query_annotations() -> Result<()> {
    let input = input()?;
    let q = outputs(&[("q", "q", "out", "q/out")])?;
    let results = bodies(&input, &q)?;
    assert_eq!(results.len(), 5, "Noop produces no outbound change");
    // Query output is ordinary graph data: no annotation changes the projection.
    assert_eq!(results, bodies(&unannotated(&input), &q)?);
    let operations: Vec<_> = results
        .iter()
        .map(|body| body["operation"].clone())
        .collect();
    assert_eq!(
        operations,
        ["ADD", "REPLACE", "DELETE", "REPLACE", "REPLACE"]
    );
    for body in &results {
        assert_eq!(
            body["output"],
            json!({"alias":"q","component":"q","port":"out"})
        );
        assert!(body.get("producer").is_none());
        assert_eq!(body["sequence"], 42);
        assert_eq!(body["element"], json!({"kind":"node","labels":["Result"]}));
        assert_eq!(
            body["identity"]["namespace"],
            "test/native-network-wire/q/out"
        );
        for stale in [
            "queryId",
            "query_id",
            "row_signature",
            "rowSignature",
            "metadata",
        ] {
            assert!(body.get(stale).is_none(), "{stale}");
        }
    }
    // Row identities are the evaluator's opaque keys, hex encoded, not hashes.
    let ids: Vec<_> = results
        .iter()
        .map(|body| body["identity"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        [
            "000000000000000b",
            "000000000000000c",
            "000000000000000d",
            "000000000000000e",
            "000000000000000f"
        ]
    );
    // A replacement with no before image has none, not a null row.
    assert!(results[3].get("before").is_none());
    assert_eq!(results[4]["before"], json!({"value":2}));
    assert_eq!(results[0]["after"]["value"], 3);

    let other = outputs(&[("other", "other", "out", "other/out")])?;
    assert!(wire::notifications(&input, &other).is_err());
    let mut wrong_port = input.clone();
    wrong_port.port = PortId::try_new("wrong")?;
    assert!(wire::notifications(&wrong_port, &q).is_err());
    Ok(())
}

#[test]
fn source_changes_reach_network_projection_with_full_patch_and_removal_semantics() -> Result<()> {
    let input = source_input(&[
        SourceChange::Insert {
            element: room("r1", "one"),
        },
        SourceChange::Replace {
            element: room("r1", "two"),
        },
        SourceChange::Update {
            element: room("r1", "three"),
            removed_properties: ["colour".to_owned()].into(),
        },
        SourceChange::Delete {
            metadata: drasi_core::models::ElementMetadata {
                reference: drasi_core::models::ElementReference::new("facilities-db", "r1"),
                labels: Arc::from([Arc::from("Room")]),
                effective_from: 2_000,
            },
        },
    ])?;
    let facilities = outputs(&[("facilities-db", "facilities-db", "out", "facilities-db/out")])?;
    let bodies_ = bodies(&input, &facilities)?;
    assert_eq!(bodies_.len(), 4);
    // No query identity is invented: the output is the configured binding.
    assert!(bodies_.iter().all(|body| body["output"]
        == json!({"alias":"facilities-db","component":"facilities-db","port":"out"})
        && body["sequence"] == 7));
    let operations: Vec<_> = bodies_
        .iter()
        .map(|body| body["operation"].clone())
        .collect();
    assert_eq!(operations, ["ADD", "REPLACE", "PATCH", "DELETE"]);
    assert_eq!(bodies_[2]["removedProperties"], json!(["colour"]));
    assert!(bodies_[0].get("removedProperties").is_none());
    for body in &bodies_ {
        assert_eq!(
            body["identity"],
            json!({"namespace":"facilities-db","id":"r1"})
        );
        assert!(body.get("before").is_none(), "no before image is known");
    }
    assert_eq!(bodies_[0]["after"]["name"], "one");
    assert!(bodies_[3].get("after").is_none() && bodies_[3].get("before").is_none());
    // Configured bindings alone identify the data; several outputs, including
    // two ports of one component, share one sink and stay distinguishable.
    let shared = outputs(&[
        ("facilities-db", "facilities-db", "out", "facilities-db/out"),
        (
            "facilities-errors",
            "facilities-db",
            "errors",
            "facilities-db/errors",
        ),
        ("q", "q", "out", "q/out"),
    ])?;
    assert_eq!(
        bodies(&input, &shared)?[0]["output"],
        json!({"alias":"facilities-db","component":"facilities-db","port":"out"})
    );
    let errors = source_input_on(
        "facilities-db/errors",
        &[SourceChange::Insert {
            element: room("r9", "bad"),
        }],
    )?;
    assert_eq!(
        bodies(&errors, &shared)?[0]["output"],
        json!({"alias":"facilities-errors","component":"facilities-db","port":"errors"})
    );
    Ok(())
}

#[test]
fn untimed_identity_only_deletion_is_accepted_without_invented_metadata() -> Result<()> {
    let metadata = drasi_core::models::ElementMetadata {
        reference: drasi_core::models::ElementReference::new("facilities-db", "gone"),
        labels: Arc::from([Arc::from("Room")]),
        effective_from: 2_000,
    };
    let input = source_input_untimed(&[SourceChange::Delete { metadata }])?;
    assert!(input.envelope.system().timestamp().is_none());
    let facilities = outputs(&[("facilities-db", "facilities-db", "out", "facilities-db/out")])?;
    let body = &bodies(&input, &facilities)?[0];
    assert_eq!(body["operation"], "DELETE");
    assert_eq!(body["sequence"], 7);
    assert_eq!(
        body["identity"],
        json!({"namespace":"facilities-db","id":"gone"})
    );
    assert!(body.get("timestamp").is_none(), "no event time is invented");
    assert!(body.get("before").is_none() && body.get("after").is_none());
    // The browser frame names its own publication time, not an event time.
    let payload = drasi_computation_sink_sse::sse_payload(&input, &facilities)?;
    assert!(payload["publishedAt"].as_i64().is_some());
    assert!(payload["changes"][0].get("timestamp").is_none());
    Ok(())
}

#[test]
fn native_sse_sends_truthful_changes_for_every_producer_kind() -> Result<()> {
    let input = input()?;
    let queries = outputs(&[("q", "q", "out", "q/out")])?;
    let payload = drasi_computation_sink_sse::sse_payload(&input, &queries)?;
    assert_eq!(
        payload["output"],
        json!({"alias":"q","component":"q","port":"out"})
    );
    assert_eq!(payload["sequence"], 42);
    assert!(payload.get("queryId").is_none() && payload.get("results").is_none());
    let changes = payload["changes"].as_array().context("changes")?;
    assert_eq!(changes.len(), 5);
    assert_eq!(changes[0]["operation"], "ADD");
    assert_eq!(changes[0]["after"]["value"], 3);
    assert_eq!(changes[1]["operation"], "REPLACE");
    assert_eq!(changes[1]["before"], json!({"value":2}));
    assert_eq!(changes[2]["operation"], "DELETE");
    assert!(changes[3].get("before").is_none());
    assert!(payload["publishedAt"].as_i64().is_some());
    assert!(payload.get("timestamp").is_none() && payload.get("producer").is_none());
    assert_eq!(
        drasi_computation_sink_sse::sse_payload(&unannotated(&input), &queries)?["changes"],
        payload["changes"]
    );

    // A source is bound and decoded the same way, and a patch or an
    // identity-only delete is sent as such for the browser's row state to apply.
    let source = source_input(&[
        SourceChange::Insert {
            element: room("r1", "one"),
        },
        SourceChange::Update {
            element: room("r1", "two"),
            removed_properties: ["colour".to_owned()].into(),
        },
        SourceChange::Delete {
            metadata: drasi_core::models::ElementMetadata {
                reference: drasi_core::models::ElementReference::new("facilities-db", "r1"),
                labels: Arc::from([Arc::from("Room")]),
                effective_from: 2_000,
            },
        },
    ])?;
    let sources = outputs(&[("facilities-db", "facilities-db", "out", "facilities-db/out")])?;
    let payload = drasi_computation_sink_sse::sse_payload(&source, &sources)?;
    assert_eq!(payload["output"]["component"], "facilities-db");
    let changes = payload["changes"].as_array().context("changes")?;
    assert_eq!(changes[0]["after"]["name"], "one");
    assert_eq!(changes[1]["operation"], "PATCH");
    assert_eq!(changes[1]["removedProperties"], json!(["colour"]));
    assert_eq!(changes[2]["operation"], "DELETE");
    assert_eq!(
        changes[2]["identity"],
        json!({"namespace":"facilities-db","id":"r1"})
    );
    assert!(changes[2].get("before").is_none());

    let wrong_query = outputs(&[("other", "other", "out", "other/out")])?;
    assert!(drasi_computation_sink_sse::sse_payload(&input, &wrong_query).is_err());
    let mut wrong_port = input.clone();
    wrong_port.port = PortId::try_new("wrong")?;
    assert!(drasi_computation_sink_sse::sse_payload(&wrong_port, &queries).is_err());
    let progress = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: QueryChangeCodec::progress_envelope(
            &ResultNodeProjector::try_new("test/native-network-wire/q/out", None)?,
            "q",
            42,
            1,
            &ComponentId::try_new("q")?,
            SystemMetadata::new(StreamId::try_new("q/out")?, 43),
        )?,
    };
    assert!(drasi_computation_sink_sse::sse_payload(&progress, &queries).is_err());
    let due = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_futures_due(
            StreamId::try_new("q/out")?,
            44,
            chrono::Utc::now(),
        )?,
    };
    assert!(drasi_computation_sink_sse::sse_payload(&due, &queries).is_err());
    Ok(())
}

#[test]
fn unsupported_profile_axes_and_invalid_headers_are_explicit_errors() -> Result<()> {
    assert_eq!(
        HttpSourceConfig::from_value(json!({"stream":"facilities-db/out"}))?.port,
        9000
    );
    assert_eq!(
        GrpcSourceConfig::from_value(json!({"stream":"facilities-db/out"}))?.port,
        50051
    );
    assert!(HttpSourceConfig::from_value(json!({"stream":"s","adaptiveEnabled":true})).is_ok());
    for config in [
        json!({"stream":"s","adaptiveEnabled":true,"adaptiveMinBatchSize":0}),
        json!({"stream":"s","ingressCapacity":0}),
        json!({"stream":"s","durability":{}}),
        json!({"stream":"s","timeoutMs":0}),
    ] {
        assert!(HttpSourceConfig::from_value(config).is_err());
    }
    let http = HttpSinkConfig::parse(
        json!({"outputs":{"q":{"component":"q","port":"out"}},"url":"http://localhost:9001/reaction"}),
    )?;
    assert_eq!(
        http.outputs
            .iter()
            .map(|(alias, _)| alias)
            .collect::<Vec<_>>(),
        ["q"]
    );
    assert!(!http.headers.contains_key("x-drasi-component"));
    assert!(HttpSinkConfig::parse(
        json!({"outputs":{"q":{"component":"q","port":"out"}},"url":"http://localhost","outputTemplates":{}})
    )
    .is_ok());
    for config in [
        json!({"outputs":{"q":{"component":"q","port":"out"}},"url":"http://localhost","outputTemplates":{"defaultTemplte":{}}}),
        json!({"outputs":{"q":{"component":"q","port":"out"}},"url":"http://localhost","headers":{"x-drasi-component":"other"}}),
        json!({"outputs":{"q":{"component":"q","port":"out"}},"url":"http://localhost","headers":{"x-bad":"line\nbreak"}}),
    ] {
        assert!(HttpSinkConfig::parse(config).is_err());
    }
    let grpc = GrpcSinkConfig::parse(json!({"outputs":{"q":{"component":"q","port":"out"}}}))?;
    assert!(grpc.metadata.is_empty());
    assert_eq!(grpc.max_retries, 5);
    assert_eq!(
        GrpcSinkConfig::parse(
            json!({"outputs":{"q":{"component":"q","port":"out"}},"batchSize":2})
        )?
        .batch_size,
        2
    );
    assert!(GrpcSinkConfig::parse(
        json!({"outputs":{"q":{"component":"q","port":"out"}},"batching":{"mode":"adaptive"}})
    )
    .is_ok());
    for config in [
        json!({"outputs":{"q":{"component":"q","port":"out"}},"batchSize":0}),
        json!({"outputs":{"q":{"component":"q","port":"out"}},"batching":{"mode":"adaptive","adaptiveMinBatchSize":0}}),
        json!({"outputs":{"q":{"component":"q","port":"out"}},"metadata":{"x-drasi-port":"wrong"}}),
        json!({"outputs":{"q":{"component":"q","port":"out"}},"connectionRetryAttempts":0}),
    ] {
        assert!(GrpcSinkConfig::parse(config).is_err());
    }
    Ok(())
}
