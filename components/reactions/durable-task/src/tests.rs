// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    reactions::common::{OperationType, TemplateRouting},
    Reaction,
};
use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;
use serde_json::{json, Value};

use crate::{
    descriptor::DurableTaskReactionDescriptor,
    dispatch::{handlebars, prepare, Dispatch},
    Action, Authentication, DurableTaskQueryConfig, DurableTaskReaction, DurableTaskReactionConfig,
    DurableTaskTemplate, TargetPolicy,
};

mod rpc;

#[test]
fn configured_recovery_policy_is_reported_to_the_host() {
    let reaction = DurableTaskReaction::builder("r")
        .with_queries(vec!["orders".into()])
        .with_config(config())
        .with_recovery_policy(drasi_lib::ReactionRecoveryPolicy::AutoSkipGap)
        .build()
        .unwrap();
    assert_eq!(
        reaction.default_recovery_policy(),
        drasi_lib::ReactionRecoveryPolicy::AutoSkipGap
    );
}

fn start(instance_id: Option<&str>) -> DurableTaskTemplate {
    DurableTaskTemplate::with_extension(
        "",
        Action::Start {
            orchestration: "Workflow".into(),
            version: String::new(),
            instance_id: instance_id.map(Into::into),
            on_conflict: TargetPolicy::Error,
        },
    )
}

fn config() -> DurableTaskReactionConfig {
    DurableTaskReactionConfig {
        endpoint: "http://127.0.0.1:8080".into(),
        task_hub: "default".into(),
        producer_id: "test-v1".into(),
        authentication: Authentication::None,
        retry_delay_ms: 1,
        default_template: Some(DurableTaskQueryConfig {
            added: Some(start(None)),
            updated: Some(start(None)),
            deleted: Some(start(None)),
        }),
        ..Default::default()
    }
}

fn emission(sequence: u64, results: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(
        "orders".into(),
        sequence,
        chrono::Utc::now(),
        results,
        Default::default(),
    )
}

fn add(id: &str) -> ResultDiff {
    ResultDiff::Add {
        data: json!({"id":id}),
        row_signature: 17,
    }
}

#[test]
fn builder_validates_endpoint_auth_routes_and_templates() {
    let build = |config| DurableTaskReaction::new("r", vec!["orders".into()], config);
    assert!(build(config()).is_ok());
    assert!(build(DurableTaskReactionConfig::default()).is_err());
    for endpoint in [
        "http://remote.example",
        "https://localhost",
        "http://user@localhost",
        "http://localhost/path",
    ] {
        assert!(build(DurableTaskReactionConfig {
            endpoint: endpoint.into(),
            ..config()
        })
        .is_err());
    }
    let mut c = config();
    c.authentication = Authentication::Identity;
    assert!(build(c.clone()).is_err());
    c.endpoint = "https://scheduler.example.com".into();
    assert!(build(c).is_ok());
    let mut c = config();
    c.routes
        .insert("unknown".into(), DurableTaskQueryConfig::default());
    assert!(build(c).is_err());
    let mut c = config();
    c.default_template.as_mut().unwrap().added = Some(start(Some("{{#if")));
    assert!(build(c).is_err());
}

#[test]
fn generated_ids_are_stable_and_distinguish_producers_rows_and_operations() {
    let c = config();
    let result = emission(7, vec![add("a"), add("a")]);
    let first = prepare(&c, "r", &result, &handlebars()).unwrap();
    assert_eq!(first, prepare(&c, "r", &result, &handlebars()).unwrap());
    assert_ne!(first[0], first[1]);
    let id = |operation: &Dispatch| match operation {
        Dispatch::Start { instance_id, .. } => instance_id.clone(),
        _ => panic!("expected start"),
    };
    assert_ne!(id(&first[0]), id(&first[1]));
    assert_ne!(
        id(&first[0]),
        id(&prepare(&c, "other-reaction", &result, &handlebars()).unwrap()[0])
    );
    let mut next_sequence = result.clone();
    next_sequence.sequence += 1;
    assert_ne!(
        id(&first[0]),
        id(&prepare(&c, "r", &next_sequence, &handlebars()).unwrap()[0])
    );
    let changed = DurableTaskReactionConfig {
        producer_id: "test-v2".into(),
        ..c
    };
    assert_ne!(
        first,
        prepare(&changed, "r", &result, &handlebars()).unwrap()
    );
}

#[test]
fn all_operations_use_selected_actions_and_canonical_envelopes() {
    let c = config();
    let results = emission(
        3,
        vec![
            add("a"),
            ResultDiff::Update {
                before: json!({"id":"a","n":1}),
                after: json!({"id":"a","n":2}),
                data: json!({"n":2}),
                grouping_keys: None,
                row_signature: 17,
            },
            ResultDiff::Delete {
                data: json!({"id":"a","n":2}),
                row_signature: 17,
            },
            ResultDiff::Aggregation {
                before: None,
                after: json!({"count":2}),
                row_signature: 19,
            },
            ResultDiff::Noop,
        ],
    );
    let dispatches = prepare(&c, "r", &results, &handlebars()).unwrap();
    assert_eq!(dispatches.len(), 4);
    let bodies: Vec<Value> = dispatches
        .iter()
        .map(|item| match item {
            Dispatch::Start { input, .. } => serde_json::from_str(input).unwrap(),
            _ => panic!("all operations were explicitly configured to start"),
        })
        .collect();
    assert_eq!(bodies[0]["operation"], "ADD");
    assert!(bodies[0].get("before").is_none());
    assert_eq!(bodies[1]["before"]["n"], 1);
    assert_eq!(bodies[1]["after"]["n"], 2);
    assert_eq!(bodies[2]["operation"], "DELETE");
    assert_eq!(bodies[2]["before"]["n"], 2);
    assert!(bodies[2].get("after").is_none());
    assert_eq!(bodies[3]["operation"], "UPDATE");
    assert!(bodies[3].get("before").is_none());
    assert!(bodies
        .iter()
        .all(|b| b["queryId"] == "orders" && b["sequenceId"] == 3));
    assert!(prepare(&c, "r", &emission(4, vec![]), &handlebars())
        .unwrap()
        .is_empty());
}

#[test]
fn routing_uses_full_then_short_then_default_per_operation() {
    let mut c = config();
    c.routes.insert(
        "orders".into(),
        DurableTaskQueryConfig {
            added: Some(DurableTaskTemplate::with_extension("", Action::Ignore)),
            ..Default::default()
        },
    );
    c.routes.insert(
        "ns.orders".into(),
        DurableTaskQueryConfig {
            updated: Some(start(Some("full"))),
            ..Default::default()
        },
    );
    assert!(matches!(
        c.get_template_spec("ns.orders", OperationType::Add)
            .unwrap()
            .extension,
        Action::Ignore
    ));
    assert_eq!(
        c.get_template_spec("ns.orders", OperationType::Update)
            .unwrap(),
        &start(Some("full"))
    );
    assert_eq!(
        c.get_template_spec("ns.orders", OperationType::Delete)
            .unwrap(),
        &start(None)
    );
}

#[test]
fn body_failure_falls_back_but_target_failure_never_sends_elsewhere() {
    let mut c = config();
    c.default_template
        .as_mut()
        .unwrap()
        .added
        .as_mut()
        .unwrap()
        .template = "{{missing}}".into();
    let result = prepare(&c, "r", &emission(1, vec![add("a")]), &handlebars()).unwrap();
    let Dispatch::Start { input, .. } = &result[0] else {
        panic!("start expected");
    };
    assert_eq!(
        serde_json::from_str::<Value>(input).unwrap()["after"]["id"],
        "a"
    );
    c.default_template.as_mut().unwrap().added = Some(start(Some("{{after.missing}}")));
    assert!(prepare(&c, "r", &emission(1, vec![add("a")]), &handlebars()).is_err());
}

#[tokio::test]
async fn descriptor_round_trips_raw_config_and_embedded_properties() {
    let raw = json!({
        "endpoint": {"kind":"EnvironmentVariable", "name":"DRASI_DTS_TEST_UNSET_ENDPOINT", "default":"http://127.0.0.1:8080"},
        "taskHub":"default", "producerId":"v1", "authentication":"none",
        "priorityQueueCapacity":23, "recoveryPolicy":"strict",
        "defaultTemplate": {
            "added": {"action":"start","orchestration":"Workflow","instanceId":"{{after.id}}"},
            "updated": {"action":"raiseEvent","instanceId":"{{after.id}}","eventName":"changed","onMissing":"ignore"},
            "deleted": {"action":"terminate","instanceId":"{{before.id}}","recursive":true}
        }
    });
    let descriptor = DurableTaskReactionDescriptor;
    let reaction = descriptor
        .create_reaction("r", vec!["orders".into()], &raw, false)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(reaction.properties()).unwrap(), raw);
    assert!(!reaction.auto_start());
    assert!(reaction.is_durable());
    let schema: Value = serde_json::from_str(&descriptor.config_schema_json()).unwrap();
    assert!(schema.get(descriptor.config_schema_name()).is_some());

    let embedded = DurableTaskReaction::builder("r")
        .with_query("orders")
        .with_config(config())
        .with_priority_queue_capacity(42)
        .build()
        .unwrap();
    let saved = serde_json::to_value(embedded.properties()).unwrap();
    assert_eq!(saved["priorityQueueCapacity"], 42);
    let restored = descriptor
        .create_reaction("r", vec!["orders".into()], &saved, true)
        .await
        .unwrap();
    assert_eq!(restored.properties(), embedded.properties());
    for invalid in [
        json!({"action":"raiseEvent", "instanceId":"x"}),
        json!({"action":"start", "orchestration":"x", "onMissing":"ignore"}),
        json!({"action":"typo"}),
    ] {
        let mut invalid_config = raw.clone();
        invalid_config["defaultTemplate"]["updated"] = invalid;
        assert!(descriptor
            .create_reaction("r", vec!["orders".into()], &invalid_config, true)
            .await
            .is_err());
    }
}
