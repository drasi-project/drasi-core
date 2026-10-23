// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::{config::*, descriptor::TemporalReactionDescriptor, render, TemporalReaction};
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    ComponentStatus, Reaction,
};
use drasi_plugin_sdk::ReactionPluginDescriptor;
use serde_json::{json, Value};

fn config(spec: Value) -> TemporalReactionConfig {
    serde_json::from_value(json!({"outputTemplates":{"routes":{"q":{"added":spec}}}})).unwrap()
}
fn signal() -> Value {
    json!({"action":"signal","workflowId":"{{row.id}}","signalName":"notify","template":"{{json after}}"})
}
fn result(diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult {
        query_id: "q".into(),
        sequence: 1,
        timestamp: chrono::Utc::now(),
        results: diffs,
        metadata: Default::default(),
        profiling: None,
    }
}
fn add(row: Value) -> ResultDiff {
    ResultDiff::Add {
        data: row,
        row_signature: 1,
    }
}

#[test]
fn action_shapes_reject_unrelated_fields_and_unknown_operations() {
    for spec in [
        json!({"action":"signal","workflowId":"x","signalName":"s","workflowType":"Wrong"}),
        json!({"action":"start","workflowId":"x","workflowType":"w","taskQueue":"q","runId":"r"}),
        json!({"action":"cancel","workflowId":"x","signalName":"s"}),
        json!({"action":"update","workflowId":"x","updateName":"u","waitFor":"sent"}),
        json!({"action":"signal_with_start","workflowId":"x"}),
        json!({"action":"terminate","workflowId":"x"}),
        json!({"workflowId":"x","signalName":"s"}),
    ] {
        assert!(
            serde_json::from_value::<TemporalReactionConfig>(
                json!({"outputTemplates":{"routes":{"q":{"added":spec}}}})
            )
            .is_err(),
            "{spec}"
        );
    }
    assert!(serde_json::from_value::<TemporalReactionConfig>(
        json!({"outputTemplates":{"routes":{"q":{"unsupported":signal()}}}})
    )
    .is_err());
    let cancel = config(json!({"action":"cancel","workflowId":"x","template":"{}"}));
    assert!(cancel.validate(&["q".into()]).is_err());
}

#[test]
fn explicit_start_and_update_defaults_are_safe() {
    let c = config(json!({"action":"start","workflowId":"x","workflowType":"w","taskQueue":"q"}));
    assert!(matches!(
        c.output_templates.routes["q"]
            .added
            .as_ref()
            .unwrap()
            .extension,
        Action::Start {
            conflict_policy: ConflictPolicy::Fail,
            reuse_policy: ReusePolicy::RejectDuplicate,
            ..
        }
    ));
    let c = config(json!({"action":"update","workflowId":"x","updateName":"u"}));
    assert!(matches!(
        c.output_templates.routes["q"]
            .added
            .as_ref()
            .unwrap()
            .extension,
        Action::Update {
            wait_for: UpdateWait::Completed,
            ..
        }
    ));
}

#[test]
fn validates_all_templates_before_start() {
    for field in ["workflowId", "signalName", "template"] {
        let mut s = signal();
        s[field] = json!("{{");
        assert!(config(s).validate(&["q".into()]).is_err(), "{field}");
    }
    assert!(config(signal()).validate(&["unrelated".into()]).is_err());
    for field in ["workflowId", "signalName"] {
        let mut s = signal();
        s[field] = json!(" ");
        assert!(config(s).validate(&["q".into()]).is_err());
    }
}

#[test]
fn validates_connection_credentials_and_builder() {
    let mut c = config(signal());
    c.api_key = Some("secret".into());
    assert!(c.validate(&["q".into()]).is_err());
    c.endpoint = "https://host:7233".into();
    assert!(c.validate(&["q".into()]).is_ok());
    c.tls = Some(TlsConfig {
        client_identity: Some(ClientIdentity {
            certificate: "cert".into(),
            private_key: "".into(),
        }),
        ..Default::default()
    });
    assert!(c.validate(&["q".into()]).is_err());
    for endpoint in ["ftp://host", "http://host/api", "http://host?q=x"] {
        let mut c = config(signal());
        c.endpoint = endpoint.into();
        assert!(c.validate(&["q".into()]).is_err());
    }
    assert!(TemporalReaction::builder("r")
        .with_config(config(signal()))
        .build()
        .is_err());
    assert!(TemporalReaction::builder("r")
        .with_query("q")
        .with_query("q")
        .build()
        .is_err());
}

#[test]
fn rendering_supports_every_diff_and_exact_suffix_default_precedence() {
    let spec = json!({"action":"signal","workflowId":"{{row.id}}","signalName":"{{operation}}",
        "template":"{\"before\":{{json before}},\"after\":{{json after}},\"metadata\":{{json metadata}}}"});
    let c: TemporalReactionConfig = serde_json::from_value(json!({"outputTemplates":{
        "defaultTemplate":{"deleted":spec},
        "routes":{"q":{"updated":spec},"source.q":{"added":spec}}
    }}))
    .unwrap();
    let mut qr = result(vec![
        add(json!({"id":"A<&\""})),
        ResultDiff::Update {
            before: json!({"id":"old"}),
            after: json!({"id":"new"}),
            data: json!({"id":"new"}),
            grouping_keys: None,
            row_signature: 2,
        },
        ResultDiff::Delete {
            data: json!({"id":"D"}),
            row_signature: 3,
        },
        ResultDiff::Aggregation {
            before: None,
            after: json!({"id":"aggregate"}),
            row_signature: 4,
        },
        ResultDiff::Noop,
    ]);
    qr.query_id = "source.q".into();
    qr.metadata.insert("test".into(), json!(true));
    let commands = render::commands(&c, &qr).unwrap();
    assert_eq!(commands.len(), 4);
    assert_eq!(commands[0].action.workflow_id(), "A<&\"");
    assert_eq!(commands[1].action.workflow_id(), "new");
    assert_eq!(commands[2].action.workflow_id(), "D");
    assert_eq!(commands[3].payload.as_ref().unwrap()["before"], Value::Null);
    assert_eq!(
        commands[0].payload.as_ref().unwrap()["metadata"]["test"],
        true
    );
}

#[test]
fn payload_fallback_and_routing_failures_are_distinct() {
    let qr = result(vec![add(json!({"id":"A"}))]);
    for template in ["", "{{missing}}", "not-json"] {
        let mut s = signal();
        s["template"] = json!(template);
        let commands = render::commands(&config(s), &qr).unwrap();
        let payload = commands[0].payload.as_ref().unwrap();
        assert_eq!(payload["operation"], "ADD");
        assert_eq!(payload["after"]["id"], "A");
    }
    assert!(render::commands(&config(signal()), &result(vec![add(json!({}))])).is_err());
    assert!(render::commands(
        &config(signal()),
        &result(vec![add(json!({"id":"x".repeat(1001)}))])
    )
    .is_err());
    assert!(render::commands(&TemporalReactionConfig::default(), &qr)
        .unwrap()
        .is_empty());
    assert!(render::commands(&config(signal()), &result(vec![]))
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn properties_round_trip_secrets_references_and_schema() {
    let raw = json!({
        "endpoint":{"kind":"EnvironmentVariable","name":"TEMPORAL_TEST_NO_ENDPOINT","default":"https://localhost:7233"},
        "apiKey":"not-a-real-key","priorityQueueCapacity":27,
        "outputTemplates":{"routes":{"q":{"added":signal()}}}
    });
    let r = TemporalReactionDescriptor
        .create_reaction("r", vec!["q".into()], &raw, false)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(r.properties()).unwrap(), raw);
    assert!(r.is_durable());
    assert!(!r.needs_snapshot_on_fresh_start());
    let mut c = config(signal());
    c.endpoint = "https://host".into();
    c.api_key = Some("test".into());
    let embedded = TemporalReaction::builder("r")
        .with_query("q")
        .with_config(c)
        .build()
        .unwrap();
    let props = serde_json::to_value(embedded.properties()).unwrap();
    assert_eq!(props["apiKey"], "test");
    TemporalReactionDescriptor
        .create_reaction("r", vec!["q".into()], &props, true)
        .await
        .unwrap();
    let schema: Value =
        serde_json::from_str(&TemporalReactionDescriptor.config_schema_json()).unwrap();
    assert!(schema["reaction.temporal.Action"]["oneOf"].is_array());
    assert_eq!(
        schema["reaction.temporal.TemporalReactionConfig"]["properties"]["apiKey"]["x-ui:widget"],
        "password"
    );
}

#[tokio::test]
async fn uninitialized_lifecycle_reports_failure_and_stop_is_idempotent() {
    let reaction = TemporalReaction::builder("r")
        .with_query("q")
        .build()
        .unwrap();
    reaction.stop().await.unwrap();
    assert!(reaction.start().await.is_err());
    assert_eq!(reaction.status().await, ComponentStatus::Error);
    reaction.stop().await.unwrap();
    reaction.stop().await.unwrap();
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}
