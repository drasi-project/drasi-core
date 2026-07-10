// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Unit tests for the Event Grid reaction.

use std::collections::HashMap;

use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::Reaction;
use serde_json::json;

use crate::config::{
    EventGridOutputTemplates, EventGridQueryConfig, EventGridReactionConfig, EventGridSchema,
    EventGridTemplateExt, EventGridTemplateSpec, OutputFormat,
};
use crate::event::{deterministic_id, unpacked_notification, ChangeOp, EventEnvelope};
use crate::reaction::{build_events, build_handlebars};
use crate::{EventGridReaction, EventGridReactionBuilder};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn query_result(query_id: &str, sequence: u64, results: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        chrono::Utc::now(),
        results,
        HashMap::new(),
    )
}

fn add(data: serde_json::Value) -> ResultDiff {
    ResultDiff::Add {
        data,
        row_signature: 0,
    }
}

fn delete(data: serde_json::Value) -> ResultDiff {
    ResultDiff::Delete {
        data,
        row_signature: 0,
    }
}

fn update(before: serde_json::Value, after: serde_json::Value) -> ResultDiff {
    ResultDiff::Update {
        data: json!({ "before": before, "after": after }),
        before,
        after,
        grouping_keys: None,
        row_signature: 0,
    }
}

/// Build a template spec with the given template and no metadata.
fn spec(template: &str) -> EventGridTemplateSpec {
    EventGridTemplateSpec::with_extension(template, EventGridTemplateExt::default())
}

// ---------------------------------------------------------------------------
// Builder & defaults
// ---------------------------------------------------------------------------

#[test]
fn builder_defaults() {
    let r = EventGridReactionBuilder::new("test")
        .with_endpoint("https://topic.eastus-1.eventgrid.azure.net/api/events") // DevSkim: ignore DS137138
        .with_access_key("k")
        .build()
        .unwrap();
    assert_eq!(r.id(), "test");
    assert_eq!(r.type_name(), "eventgrid");
    let p = r.properties();
    assert_eq!(
        p.get("timeoutMs"),
        Some(&serde_json::Value::Number(10_000.into()))
    );
    assert_eq!(p.get("format"), Some(&json!("packed")));
    assert_eq!(p.get("schema"), Some(&json!("CloudEvents")));
}

#[test]
fn builder_retains_access_key_in_properties() {
    let r = EventGridReaction::builder("r")
        .with_endpoint("https://topic.eastus-1.eventgrid.azure.net/api/events") // DevSkim: ignore DS137138
        .with_access_key("super-secret")
        .with_query("q1")
        .build()
        .unwrap();
    let p = r.properties();
    // Persistence contract: access key must be present in properties().
    assert_eq!(p.get("accessKey"), Some(&json!("super-secret")));
}

#[test]
fn config_debug_redacts_access_key() {
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("super-secret".into()),
        ..Default::default()
    };
    let dbg = format!("{config:?}");
    assert!(!dbg.contains("super-secret"), "access key leaked: {dbg}");
    assert!(dbg.contains("REDACTED"));
}

// ---------------------------------------------------------------------------
// Config serde
// ---------------------------------------------------------------------------

#[test]
fn config_serializes_camel_case() {
    let c = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        ..Default::default()
    };
    let v = serde_json::to_value(&c).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("endpoint"));
    assert!(obj.contains_key("accessKey"));
    assert!(obj.contains_key("timeoutMs"));
    assert!(!obj.contains_key("outputTemplates")); // skipped when None
}

#[test]
fn config_deserializes_format_and_schema() {
    let json = json!({
        "endpoint": "https://x.eventgrid.azure.net/api/events", // DevSkim: ignore DS137138
        "accessKey": "k",
        "schema": "EventGrid",
        "format": "unpacked"
    });
    let c: EventGridReactionConfig = serde_json::from_value(json).unwrap();
    assert_eq!(c.schema, EventGridSchema::EventGrid);
    assert_eq!(c.format, OutputFormat::Unpacked);
}

#[test]
fn config_rejects_unknown_fields() {
    let json = json!({
        "endpoint": "https://x.eventgrid.azure.net/api/events", // DevSkim: ignore DS137138
        "bogus": true
    });
    let result: Result<EventGridReactionConfig, _> = serde_json::from_value(json);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// validate()
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_empty_endpoint() {
    let c = EventGridReactionConfig {
        access_key: Some("k".into()),
        ..Default::default()
    };
    assert!(c.validate(&[], None).is_err());
}

#[test]
fn validate_rejects_non_https_scheme() {
    let c = EventGridReactionConfig {
        endpoint: "ftp://x/api/events".into(),
        access_key: Some("k".into()),
        ..Default::default()
    };
    assert!(c.validate(&[], None).is_err());
}

#[test]
fn validate_accepts_http_for_local_testing() {
    let c = EventGridReactionConfig {
        endpoint: "http://localhost:1234/api/events".into(),
        access_key: Some("k".into()),
        ..Default::default()
    };
    assert!(c.validate(&[], None).is_ok());
}

#[test]
fn validate_template_format_requires_templates() {
    let c = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Template,
        ..Default::default()
    };
    assert!(c.validate(&["q1".to_string()], None).is_err());
}

#[test]
fn validate_rejects_route_for_unknown_query() {
    let mut routes = HashMap::new();
    routes.insert(
        "unknown".to_string(),
        EventGridQueryConfig {
            added: Some(spec("{{after.id}}")),
            ..Default::default()
        },
    );
    let c = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Template,
        output_templates: Some(EventGridOutputTemplates {
            default_template: None,
            routes,
        }),
        ..Default::default()
    };
    assert!(c.validate(&["q1".to_string()], None).is_err());
}

// ---------------------------------------------------------------------------
// Deterministic ids
// ---------------------------------------------------------------------------

#[test]
fn deterministic_id_is_stable() {
    let a = deterministic_id("r", "q", 5, "i", 0);
    let b = deterministic_id("r", "q", 5, "i", 0);
    assert_eq!(a, b);
    // Different tuple → different id.
    assert_ne!(a, deterministic_id("r", "q", 5, "i", 1));
    assert_ne!(a, deterministic_id("r", "q", 6, "i", 0));
    assert_ne!(a, deterministic_id("r", "q", 5, "u", 0));
}

// ---------------------------------------------------------------------------
// Unpacked notification mapping
// ---------------------------------------------------------------------------

#[test]
fn unpacked_notification_maps_operations() {
    let qr = query_result(
        "orders",
        1,
        vec![
            add(json!({ "id": 1 })),
            update(json!({ "id": 1 }), json!({ "id": 1, "n": 2 })),
            delete(json!({ "id": 1 })),
            ResultDiff::Noop,
        ],
    );

    let (op, data) = unpacked_notification(&qr, &qr.results[0]).unwrap();
    assert_eq!(op, ChangeOp::Insert);
    assert_eq!(data["op"], "i");
    assert_eq!(data["payload"]["after"]["id"], 1);
    assert_eq!(data["payload"]["source"]["queryId"], "orders");

    let (op, data) = unpacked_notification(&qr, &qr.results[1]).unwrap();
    assert_eq!(op, ChangeOp::Update);
    assert_eq!(data["op"], "u");
    assert!(data["payload"]["before"].is_object());
    assert!(data["payload"]["after"].is_object());

    let (op, data) = unpacked_notification(&qr, &qr.results[2]).unwrap();
    assert_eq!(op, ChangeOp::Delete);
    assert_eq!(data["op"], "d");
    assert!(data["payload"]["before"].is_object());

    assert!(unpacked_notification(&qr, &qr.results[3]).is_none());
}

#[test]
fn unpacked_notification_aggregation_is_update() {
    let qr = query_result(
        "agg",
        1,
        vec![ResultDiff::Aggregation {
            before: Some(json!({ "sum": 1 })),
            after: json!({ "sum": 2 }),
            row_signature: 0,
        }],
    );
    let (op, data) = unpacked_notification(&qr, &qr.results[0]).unwrap();
    assert_eq!(op, ChangeOp::Update);
    assert_eq!(data["op"], "u");
    assert_eq!(data["payload"]["after"]["sum"], 2);
}

// ---------------------------------------------------------------------------
// Schema encoding (EventEnvelope::to_value)
// ---------------------------------------------------------------------------

fn sample_envelope() -> EventEnvelope {
    let mut metadata = serde_json::Map::new();
    metadata.insert("category".into(), json!("inventory"));
    EventEnvelope {
        id: "id-1".into(),
        subject: "orders".into(),
        event_type: "Drasi.ChangeEvent".into(),
        time: "2026-07-10T00:00:00+00:00".into(),
        data: json!({ "op": "i" }),
        metadata,
    }
}

#[test]
fn cloudevents_schema_fields_and_extensions() {
    let (v, dropped) = sample_envelope().to_value(EventGridSchema::CloudEvents);
    assert!(!dropped);
    assert_eq!(v["id"], "id-1");
    assert_eq!(v["source"], "orders");
    assert_eq!(v["type"], "Drasi.ChangeEvent");
    assert_eq!(v["specversion"], "1.0");
    assert_eq!(v["subject"], "orders");
    // Metadata flattened as extension attribute.
    assert_eq!(v["category"], "inventory");
}

#[test]
fn eventgrid_schema_fields_and_dropped_metadata() {
    let (v, dropped) = sample_envelope().to_value(EventGridSchema::EventGrid);
    assert!(dropped, "metadata should be reported as dropped");
    assert_eq!(v["subject"], "orders");
    assert_eq!(v["eventType"], "Drasi.ChangeEvent");
    assert_eq!(v["dataVersion"], "1");
    // Native EventGrid schema does not carry extension attributes.
    assert!(v.get("category").is_none());
}

// ---------------------------------------------------------------------------
// build_events (format dispatch)
// ---------------------------------------------------------------------------

#[test]
fn build_events_packed_emits_single_event() {
    let hb = build_handlebars();
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Packed,
        ..Default::default()
    };
    let qr = query_result(
        "orders",
        1,
        vec![add(json!({ "id": 1 })), add(json!({ "id": 2 }))],
    );
    let events = build_events("r", &config, &hb, &qr, "r");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].subject, "orders");
    // Packed data is the serialized QueryResult.
    assert_eq!(events[0].data["query_id"], "orders");
}

#[test]
fn build_events_packed_skips_noop_only() {
    let hb = build_handlebars();
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Packed,
        ..Default::default()
    };
    let qr = query_result("orders", 1, vec![ResultDiff::Noop]);
    let events = build_events("r", &config, &hb, &qr, "r");
    assert!(events.is_empty());
}

#[test]
fn build_events_unpacked_emits_per_diff() {
    let hb = build_handlebars();
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Unpacked,
        ..Default::default()
    };
    let qr = query_result(
        "orders",
        1,
        vec![
            add(json!({ "id": 1 })),
            delete(json!({ "id": 2 })),
            ResultDiff::Noop,
        ],
    );
    let events = build_events("r", &config, &hb, &qr, "r");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data["op"], "i");
    assert_eq!(events[1].data["op"], "d");
}

#[test]
fn build_events_template_renders_data_and_metadata() {
    let hb = build_handlebars();
    let mut routes = HashMap::new();
    routes.insert(
        "orders".to_string(),
        EventGridQueryConfig {
            added: Some(EventGridTemplateSpec::with_extension(
                r#"{ "productId": {{after.id}} }"#,
                EventGridTemplateExt {
                    metadata: HashMap::from([("category".to_string(), "inventory".to_string())]),
                },
            )),
            ..Default::default()
        },
    );
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Template,
        output_templates: Some(EventGridOutputTemplates {
            default_template: None,
            routes,
        }),
        ..Default::default()
    };
    let qr = query_result("orders", 1, vec![add(json!({ "id": 42 }))]);
    let events = build_events("r", &config, &hb, &qr, "r");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["productId"], 42);
    assert_eq!(
        events[0].metadata.get("category"),
        Some(&json!("inventory"))
    );
}

#[test]
fn build_events_template_invalid_json_falls_back_to_unpacked() {
    let hb = build_handlebars();
    let mut routes = HashMap::new();
    routes.insert(
        "orders".to_string(),
        EventGridQueryConfig {
            // Not valid JSON after rendering.
            added: Some(spec("not json {{after.id}}")),
            ..Default::default()
        },
    );
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Template,
        output_templates: Some(EventGridOutputTemplates {
            default_template: None,
            routes,
        }),
        ..Default::default()
    };
    let qr = query_result("orders", 1, vec![add(json!({ "id": 7 }))]);
    let events = build_events("r", &config, &hb, &qr, "r");
    assert_eq!(events.len(), 1);
    // Fallback = unpacked notification.
    assert_eq!(events[0].data["op"], "i");
    assert_eq!(events[0].data["payload"]["after"]["id"], 7);
}

#[test]
fn build_events_template_missing_template_falls_back_to_unpacked() {
    let hb = build_handlebars();
    // Route exists for a different op (updated), not for added.
    let mut routes = HashMap::new();
    routes.insert(
        "orders".to_string(),
        EventGridQueryConfig {
            updated: Some(spec(r#"{ "x": 1 }"#)),
            ..Default::default()
        },
    );
    let config = EventGridReactionConfig {
        endpoint: "https://x.eventgrid.azure.net/api/events".into(), // DevSkim: ignore DS137138
        access_key: Some("k".into()),
        format: OutputFormat::Template,
        output_templates: Some(EventGridOutputTemplates {
            default_template: None,
            routes,
        }),
        ..Default::default()
    };
    let qr = query_result("orders", 1, vec![add(json!({ "id": 9 }))]);
    let events = build_events("r", &config, &hb, &qr, "r");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["op"], "i");
}

// ---------------------------------------------------------------------------
// Recovery hooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recovery_defaults_are_non_durable_autoskipgap() {
    use drasi_lib::recovery::ReactionRecoveryPolicy;
    let r = EventGridReaction::builder("r")
        .with_endpoint("https://x.eventgrid.azure.net/api/events") // DevSkim: ignore DS137138
        .with_access_key("k")
        .build()
        .unwrap();
    assert!(!r.is_durable());
    assert!(!r.needs_snapshot_on_fresh_start());
    assert_eq!(
        r.default_recovery_policy(),
        ReactionRecoveryPolicy::AutoSkipGap
    );
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn descriptor_preserves_raw_config_for_lossless_properties() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::EventGridReactionDescriptor;
    let cfg = json!({
        "endpoint": "https://topic.eastus-1.eventgrid.azure.net/api/events", // DevSkim: ignore DS137138
        "accessKey": "raw-secret",
        "schema": "EventGrid",
        "format": "unpacked",
        "timeoutMs": 1500,
        "priorityQueueCapacity": 250
    });
    let r = d
        .create_reaction("r", vec!["q1".to_string()], &cfg, true)
        .await
        .unwrap();
    let p = r.properties();
    // properties() returns the raw config JSON verbatim (set_raw_config), so
    // every field — including priorityQueueCapacity, which is not a config
    // field — round-trips losslessly.
    assert_eq!(p.get("accessKey"), Some(&json!("raw-secret")));
    assert_eq!(p.get("schema"), Some(&json!("EventGrid")));
    assert_eq!(p.get("format"), Some(&json!("unpacked")));
    assert_eq!(p.get("timeoutMs"), Some(&json!(1500)));
    assert_eq!(p.get("priorityQueueCapacity"), Some(&json!(250)));
}

#[tokio::test]
async fn descriptor_builds_with_minimal_config() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::EventGridReactionDescriptor;
    let cfg = json!({
        "endpoint": "https://topic.eastus-1.eventgrid.azure.net/api/events", // DevSkim: ignore DS137138
        "accessKey": "k"
    });
    let r = d
        .create_reaction("r", vec!["q1".to_string()], &cfg, true)
        .await
        .unwrap();
    assert_eq!(r.type_name(), "eventgrid");
    assert_eq!(r.query_ids(), vec!["q1".to_string()]);
}
