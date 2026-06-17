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

//! Unit tests for the HTTP reaction.

use std::collections::HashMap;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;

use crate::config::{
    resolve_http_url, AdaptiveBatchConfig, HttpCallExt, HttpOutputTemplates, HttpQueryConfig,
    HttpReactionConfig, OperationType, TemplateRouting, TemplateSpec,
};
use crate::output::DefaultChangeNotification;
use crate::process::{build_handlebars, render_batch_item};
use crate::{HttpReaction, HttpReactionBuilder};

// ---------------------------------------------------------------------------
// Builder & defaults
// ---------------------------------------------------------------------------

#[test]
fn builder_defaults() {
    let r = HttpReactionBuilder::new("test").build().unwrap();
    assert_eq!(r.id(), "test");
    let p = r.properties();
    assert_eq!(
        p.get("baseUrl"),
        Some(&serde_json::Value::String("http://localhost".to_string()))
    );
    assert_eq!(
        p.get("timeoutMs"),
        Some(&serde_json::Value::Number(5000.into()))
    );
    // Adaptive must NOT appear unless enabled
    assert!(!p.contains_key("adaptive") || p.get("adaptive") == Some(&serde_json::Value::Null));
}

#[test]
fn builder_with_queries_and_token() {
    let r = HttpReaction::builder("r")
        .with_base_url("https://api.example.com") // DevSkim: ignore DS137138
        .with_token("secret")
        .with_timeout_ms(7000)
        .with_query("q1")
        .with_query("q2")
        .build()
        .unwrap();
    assert_eq!(r.query_ids(), vec!["q1", "q2"]);
    let p = r.properties();
    assert_eq!(
        p.get("baseUrl"),
        Some(&serde_json::Value::String(
            "https://api.example.com".to_string() // DevSkim: ignore DS137138
        ))
    );
    assert_eq!(
        p.get("timeoutMs"),
        Some(&serde_json::Value::Number(7000.into()))
    );
}

#[test]
fn builder_with_adaptive_enables_adaptive_block_in_properties() {
    let r = HttpReaction::builder("r")
        .with_adaptive_defaults()
        .with_batch_endpoint("/batch")
        .build()
        .unwrap();
    let p = r.properties();
    let adaptive = p.get("adaptive").expect("adaptive should be present");
    let obj = adaptive.as_object().expect("adaptive should be an object");
    assert!(obj.contains_key("adaptiveMinBatchSize"));
    assert!(obj.contains_key("adaptiveMaxBatchSize"));
    assert!(obj.contains_key("adaptiveWindowSize"));
    assert!(obj.contains_key("adaptiveBatchTimeoutMs"));
    assert_eq!(
        p.get("batchEndpoint"),
        Some(&serde_json::Value::String("/batch".to_string()))
    );
}

#[test]
fn builder_with_output_templates_round_trip() {
    let mut routes = HashMap::new();
    routes.insert(
        "q1".to_string(),
        HttpQueryConfig {
            added: Some(TemplateSpec {
                template: "{{after.id}}".to_string(),
                extension: HttpCallExt {
                    url: "/added".to_string(),
                    method: "POST".to_string(),
                    headers: HashMap::new(),
                },
            }),
            updated: None,
            deleted: None,
        },
    );
    let templates = HttpOutputTemplates {
        default_template: None,
        routes,
    };
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_output_templates(templates)
        .build()
        .unwrap();
    let p = r.properties();
    let templates = p
        .get("outputTemplates")
        .expect("outputTemplates should appear");
    let routes = templates
        .get("routes")
        .and_then(|v| v.as_object())
        .expect("routes object");
    assert!(routes.contains_key("q1"));
}

#[test]
fn builder_with_query_template_adds_to_routes() {
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template(
            "q1",
            HttpQueryConfig {
                added: Some(TemplateSpec {
                    template: String::new(),
                    extension: HttpCallExt {
                        url: "/q1".to_string(),
                        method: "POST".to_string(),
                        headers: HashMap::new(),
                    },
                }),
                ..Default::default()
            },
        )
        .build()
        .unwrap();
    let p = r.properties();
    let routes = p
        .get("outputTemplates")
        .and_then(|t| t.get("routes"))
        .and_then(|r| r.as_object())
        .expect("routes object");
    assert!(routes.contains_key("q1"));
}

// ---------------------------------------------------------------------------
// Config: serialization + TemplateRouting
// ---------------------------------------------------------------------------

#[test]
fn config_default_camel_case() {
    let c = HttpReactionConfig::default();
    let v = serde_json::to_value(&c).unwrap();
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("baseUrl"));
    assert!(obj.contains_key("timeoutMs"));
    assert!(!obj.contains_key("outputTemplates")); // skipped when None
    assert!(!obj.contains_key("adaptive"));
    assert!(!obj.contains_key("batchEndpoint"));
}

#[test]
fn config_deserialize_with_output_templates() {
    let json = serde_json::json!({
        "baseUrl": "http://example.com",
        "outputTemplates": {
            "defaultTemplate": {
                "added": {
                    "template": "{{after.id}}",
                    "url": "/default",
                    "method": "POST"
                }
            },
            "routes": {
                "q1": {
                    "added": {
                        "template": "",
                        "url": "/q1",
                        "method": "POST"
                    }
                }
            }
        }
    });

    let c: HttpReactionConfig = serde_json::from_value(json).unwrap();
    assert_eq!(c.base_url, "http://example.com");
    assert!(c.output_templates.is_some());
    let templates = c.output_templates.unwrap();
    assert!(templates.default_template.is_some());
    assert!(templates.routes.contains_key("q1"));
}

#[test]
fn config_deserialize_with_adaptive_camel_case() {
    let json = serde_json::json!({
        "baseUrl": "http://example.com",
        "adaptive": {
            "adaptiveMinBatchSize": 5,
            "adaptiveMaxBatchSize": 500,
            "adaptiveWindowSize": 20,
            "adaptiveBatchTimeoutMs": 250
        },
        "batchEndpoint": "/batch"
    });

    let c: HttpReactionConfig = serde_json::from_value(json).unwrap();
    assert_eq!(c.base_url, "http://example.com");
    let a = c.adaptive.expect("adaptive should be present");
    assert_eq!(a.adaptive_min_batch_size, 5);
    assert_eq!(a.adaptive_max_batch_size, 500);
    assert_eq!(a.adaptive_window_size, 20);
    assert_eq!(a.adaptive_batch_timeout_ms, 250);
    assert_eq!(c.batch_endpoint.as_deref(), Some("/batch"));
}

#[test]
fn template_routing_query_specific_overrides_default() {
    let mut routes = HashMap::new();
    routes.insert(
        "q1".to_string(),
        HttpQueryConfig {
            added: Some(TemplateSpec {
                template: "q1 added".to_string(),
                extension: HttpCallExt::default(),
            }),
            ..Default::default()
        },
    );
    let default = HttpQueryConfig {
        added: Some(TemplateSpec {
            template: "default added".to_string(),
            extension: HttpCallExt::default(),
        }),
        updated: Some(TemplateSpec {
            template: "default updated".to_string(),
            extension: HttpCallExt::default(),
        }),
        ..Default::default()
    };
    let cfg = HttpReactionConfig {
        output_templates: Some(HttpOutputTemplates {
            default_template: Some(default),
            routes,
        }),
        ..Default::default()
    };

    // q1 + Add → query-specific wins
    let s = cfg.get_template_spec("q1", OperationType::Add).unwrap();
    assert_eq!(s.template, "q1 added");
    // q1 + Update → falls back to default
    let s = cfg.get_template_spec("q1", OperationType::Update).unwrap();
    assert_eq!(s.template, "default updated");
    // unknown query + Add → falls back to default
    let s = cfg.get_template_spec("other", OperationType::Add).unwrap();
    assert_eq!(s.template, "default added");
    // q1 + Delete → no spec
    assert!(cfg.get_template_spec("q1", OperationType::Delete).is_none());
}

#[test]
fn template_routing_returns_none_without_templates() {
    let cfg = HttpReactionConfig::default();
    assert!(cfg
        .get_template_spec("anything", OperationType::Add)
        .is_none());
}

// ---------------------------------------------------------------------------
// Adaptive runtime conversion
// ---------------------------------------------------------------------------

#[test]
fn adaptive_runtime_conversion_uses_window_x_100ms() {
    let public = AdaptiveBatchConfig {
        adaptive_min_batch_size: 3,
        adaptive_max_batch_size: 30,
        adaptive_window_size: 50, // 50 × 100ms = 5s
        adaptive_batch_timeout_ms: 1234,
    };
    let runtime = crate::http::to_runtime_adaptive(&public);
    assert_eq!(runtime.min_batch_size, 3);
    assert_eq!(runtime.max_batch_size, 30);
    assert_eq!(runtime.throughput_window.as_millis(), 5000);
    assert_eq!(runtime.max_wait_time.as_millis(), 1234);
    assert_eq!(runtime.min_wait_time.as_millis(), 1);
    assert!(runtime.adaptive_enabled);
}

#[test]
fn adaptive_channel_capacity_uses_recommended() {
    let public = AdaptiveBatchConfig {
        adaptive_min_batch_size: 1,
        adaptive_max_batch_size: 200,
        adaptive_window_size: 10,
        adaptive_batch_timeout_ms: 1000,
    };
    let runtime = crate::http::to_runtime_adaptive(&public);
    // recommended_channel_capacity should be max × 5 with clamping; we only
    // assert that it is > max_batch_size to confirm the helper is wired in.
    assert!(runtime.recommended_channel_capacity() > runtime.max_batch_size);
}

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------

#[test]
fn descriptor_kind_and_version() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    assert_eq!(d.kind(), "http");
    assert_eq!(d.config_version(), "2.0.0");
    assert_eq!(d.config_schema_name(), "reaction.http.HttpReactionConfig");
    assert_eq!(d.display_name(), "HTTP");
    assert!(!d.display_description().is_empty());
    assert_eq!(d.display_icon(), "http");
    // Schema must serialize and include our root type plus sub-types
    let schema = d.config_schema_json();
    assert!(schema.contains("HttpReactionConfig"));
    assert!(schema.contains("HttpOutputTemplates"));
    assert!(schema.contains("HttpQueryConfig"));
    assert!(schema.contains("HttpCallSpec"));
    assert!(schema.contains("AdaptiveBatchConfig"));
}

#[tokio::test]
async fn descriptor_rejects_unknown_top_level_fields() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "routes": {}
    });
    let err = d
        .create_reaction("my-r", vec!["q1".to_string()], &cfg, true)
        .await
        .err()
        .expect("descriptor should reject unknown fields");
    assert!(
        err.to_string().contains("unknown field"),
        "error should reject unknown fields: {err}"
    );
}

#[tokio::test]
async fn descriptor_rejects_unknown_adaptive_fields() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "adaptive": {
            "adaptive_min_batch_size": 2
        },
        "batchEndpoint": "/batch"
    });
    let err = d
        .create_reaction("my-r", vec!["q1".to_string()], &cfg, true)
        .await
        .err()
        .expect("descriptor should reject snake_case adaptive fields");
    assert!(
        err.to_string().contains("unknown field"),
        "error should reject unknown fields: {err}"
    );
}

#[tokio::test]
async fn descriptor_creates_standard_reaction() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "timeoutMs": 3000
    });
    let r = d
        .create_reaction("my-r", vec!["q1".to_string()], &cfg, true)
        .await
        .unwrap();
    assert_eq!(r.id(), "my-r");
    assert_eq!(r.type_name(), "http");
    let p = r.properties();
    assert_eq!(
        p.get("baseUrl"),
        Some(&serde_json::Value::String("http://example.com".to_string()))
    );
}

#[tokio::test]
async fn descriptor_creates_adaptive_reaction() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "adaptive": {
            "adaptiveMinBatchSize": 2,
            "adaptiveMaxBatchSize": 64,
            "adaptiveWindowSize": 10,
            "adaptiveBatchTimeoutMs": 100
        },
        "batchEndpoint": "/batch"
    });
    let r = d
        .create_reaction("my-r", vec!["q1".to_string()], &cfg, true)
        .await
        .unwrap();
    let p = r.properties();
    assert!(p.contains_key("adaptive"));
    assert_eq!(
        p.get("batchEndpoint"),
        Some(&serde_json::Value::String("/batch".to_string()))
    );
}

// ---------------------------------------------------------------------------
// Lifecycle smoke test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_start_stop_standard() {
    let r = HttpReaction::builder("test-lifecycle")
        .with_base_url("http://127.0.0.1:1") // unreachable, that's fine
        .build()
        .unwrap();
    r.start().await.unwrap();
    // Give the loop a moment to enter select
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    r.stop().await.unwrap();
    let status = r.status().await;
    assert!(matches!(
        status,
        drasi_lib::channels::ComponentStatus::Stopped
    ));
}

#[tokio::test]
async fn lifecycle_start_stop_adaptive() {
    let r = HttpReaction::builder("test-lifecycle-a")
        .with_base_url("http://127.0.0.1:1")
        .with_adaptive_defaults()
        .with_batch_endpoint("/batch")
        .build()
        .unwrap();
    r.start().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    r.stop().await.unwrap();
    let status = r.status().await;
    assert!(matches!(
        status,
        drasi_lib::channels::ComponentStatus::Stopped
    ));
}

// ---------------------------------------------------------------------------
// Builder: from_query alias
// ---------------------------------------------------------------------------

#[test]
fn from_query_is_alias_for_with_query() {
    let r = HttpReaction::builder("r")
        .from_query("a")
        .from_query("b")
        .build()
        .unwrap();
    assert_eq!(r.query_ids(), vec!["a", "b"]);
}

// ---------------------------------------------------------------------------
// Construction-time validation (build())
// ---------------------------------------------------------------------------

fn spec_with(url: &str, template: &str, headers: HashMap<String, String>) -> HttpQueryConfig {
    HttpQueryConfig {
        added: Some(TemplateSpec {
            template: template.to_string(),
            extension: HttpCallExt {
                url: url.to_string(),
                method: "POST".to_string(),
                headers,
            },
        }),
        ..Default::default()
    }
}

#[test]
fn build_fails_when_batch_endpoint_set_without_adaptive() {
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_batch_endpoint("/batch")
        .build()
        .err()
        .expect("build should fail");
    assert!(
        err.to_string().contains("batchEndpoint"),
        "error should mention batchEndpoint: {err}"
    );
}

#[test]
fn build_succeeds_when_batch_endpoint_set_with_adaptive() {
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_adaptive_defaults()
        .with_batch_endpoint("/batch")
        .build();
    assert!(r.is_ok(), "batchEndpoint with adaptive must build");
}

#[test]
fn build_fails_when_adaptive_set_without_batch_endpoint() {
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_adaptive_defaults()
        .build()
        .err()
        .expect("build should fail");
    assert!(
        err.to_string().contains("batchEndpoint"),
        "error should mention batchEndpoint: {err}"
    );
}

#[test]
fn build_succeeds_when_adaptive_combined_with_output_templates() {
    // Per-item body templating is supported in adaptive (batch) mode; the
    // url/method/headers simply do not apply to the single batch endpoint.
    let spec = spec_with("", r#"{"id":"{{after.id}}"}"#, HashMap::new());
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template("q1", spec)
        .with_adaptive_defaults()
        .with_batch_endpoint("/batch")
        .build();
    assert!(
        r.is_ok(),
        "adaptive + outputTemplates must build: {:?}",
        r.err()
    );
}

#[test]
fn build_fails_on_invalid_base_url() {
    let err = HttpReaction::builder("r")
        .with_base_url("ftp://example.com")
        .build()
        .err()
        .expect("build should fail");
    assert!(
        format!("{err:#}").contains("baseUrl"),
        "error should mention baseUrl: {err:#}"
    );
}

#[test]
fn build_fails_on_zero_timeout() {
    let err = HttpReaction::builder("r")
        .with_timeout_ms(0)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        err.to_string().contains("timeoutMs"),
        "error should mention timeoutMs: {err}"
    );
}

#[test]
fn build_fails_on_zero_priority_queue_capacity() {
    let err = HttpReaction::builder("r")
        .with_priority_queue_capacity(0)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        err.to_string().contains("priorityQueueCapacity"),
        "error should mention priorityQueueCapacity: {err}"
    );
}

#[test]
fn build_fails_on_invalid_http_method() {
    let mut qc = spec_with("/x", "", HashMap::new());
    qc.added.as_mut().unwrap().extension.method = "TRACE".to_string();
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template("q1", qc)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        format!("{err:#}").contains("HTTP method"),
        "error should mention HTTP method: {err:#}"
    );
}

#[test]
fn build_fails_on_invalid_header_name() {
    let mut headers = HashMap::new();
    headers.insert("bad header".to_string(), "value".to_string());
    let bad = spec_with("/x", "", headers);
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template("q1", bad)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        format!("{err:#}").contains("header"),
        "error should mention header: {err:#}"
    );
}

#[test]
fn build_fails_on_invalid_adaptive_ranges() {
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_adaptive(AdaptiveBatchConfig {
            adaptive_min_batch_size: 10,
            adaptive_max_batch_size: 1,
            adaptive_window_size: 10,
            adaptive_batch_timeout_ms: 100,
        })
        .with_batch_endpoint("/batch")
        .build()
        .err()
        .expect("build should fail");
    assert!(
        format!("{err:#}").contains("adaptiveMinBatchSize"),
        "error should mention adaptiveMinBatchSize: {err:#}"
    );
}

#[test]
fn build_fails_on_invalid_body_template() {
    // Unclosed block helper → Handlebars compile error.
    let bad = spec_with("/x", "{{#if cond}}", HashMap::new());
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template("q1", bad)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        format!("{err:#}").contains("template"),
        "error should mention the template: {err:#}"
    );
}

#[test]
fn build_fails_on_invalid_header_template() {
    let mut headers = HashMap::new();
    headers.insert("X-Bad".to_string(), "{{#if cond}}".to_string());
    let bad = spec_with("/x/{{after.id}}", "", headers);
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template("q1", bad)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        format!("{err:#}").contains("header"),
        "error should mention the header template: {err:#}"
    );
}

#[test]
fn build_fails_on_unknown_route_key() {
    let spec = spec_with("/x", "", HashMap::new());
    let err = HttpReaction::builder("r")
        .with_query("subscribed")
        .with_query_template("not-subscribed", spec)
        .build()
        .err()
        .expect("build should fail");
    assert!(
        err.to_string().contains("not-subscribed"),
        "error should name the offending route: {err}"
    );
}

#[test]
fn build_accepts_route_key_matching_last_dotted_segment() {
    let spec = spec_with("/x", "", HashMap::new());
    let r = HttpReaction::builder("r")
        .with_query("source.orders")
        .with_query_template("orders", spec)
        .build();
    assert!(
        r.is_ok(),
        "a route keyed by the last dotted segment must be accepted"
    );
}

// ---------------------------------------------------------------------------
// get_template_spec resolution order
// ---------------------------------------------------------------------------

fn cfg_with_routes(
    default_template: Option<HttpQueryConfig>,
    routes: HashMap<String, HttpQueryConfig>,
) -> HttpReactionConfig {
    HttpReactionConfig {
        output_templates: Some(HttpOutputTemplates {
            default_template,
            routes,
        }),
        ..Default::default()
    }
}

fn added_template(template: &str) -> HttpQueryConfig {
    HttpQueryConfig {
        added: Some(TemplateSpec {
            template: template.to_string(),
            extension: HttpCallExt::default(),
        }),
        ..Default::default()
    }
}

#[test]
fn get_template_spec_matches_full_query_id() {
    let mut routes = HashMap::new();
    routes.insert("source.q1".to_string(), added_template("full"));
    let cfg = cfg_with_routes(None, routes);
    let s = cfg
        .get_template_spec("source.q1", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "full");
}

#[test]
fn get_template_spec_falls_back_to_last_dotted_segment() {
    let mut routes = HashMap::new();
    routes.insert("q1".to_string(), added_template("segment"));
    let cfg = cfg_with_routes(None, routes);
    // Full id "source.q1" not present; resolves via last segment "q1".
    let s = cfg
        .get_template_spec("source.q1", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "segment");
}

#[test]
fn get_template_spec_prefers_full_id_over_segment() {
    let mut routes = HashMap::new();
    routes.insert("source.q1".to_string(), added_template("full"));
    routes.insert("q1".to_string(), added_template("segment"));
    let cfg = cfg_with_routes(None, routes);
    let s = cfg
        .get_template_spec("source.q1", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "full");
}

#[test]
fn get_template_spec_falls_back_to_default_template() {
    let cfg = cfg_with_routes(Some(added_template("default")), HashMap::new());
    let s = cfg
        .get_template_spec("anything", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "default");
}

#[test]
fn get_template_spec_returns_none_without_templates() {
    let cfg = HttpReactionConfig::default();
    assert!(cfg.get_template_spec("q1", OperationType::Add).is_none());
}

// ---------------------------------------------------------------------------
// Adaptive batch-item rendering (per-item body templating)
// ---------------------------------------------------------------------------

fn add_notification(
    query_id: &str,
    seq: u64,
    data: serde_json::Value,
) -> DefaultChangeNotification {
    use chrono::TimeZone;
    let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let qr = QueryResult::new(
        query_id.to_string(),
        seq,
        ts,
        vec![ResultDiff::Add {
            data,
            row_signature: 0,
        }],
        HashMap::new(),
    );
    DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap()
}

#[test]
fn render_batch_item_uses_body_template_when_present() {
    let cfg = cfg_with_routes(
        None,
        HashMap::from([(
            "q1".to_string(),
            HttpQueryConfig {
                added: Some(TemplateSpec {
                    template: r#"{"id":"{{after.id}}","kind":"templated"}"#.to_string(),
                    extension: HttpCallExt::default(),
                }),
                ..Default::default()
            },
        )]),
    );
    let n = add_notification("q1", 1, serde_json::json!({"id": "x"}));
    let hb = build_handlebars();
    let item = render_batch_item(&hb, &cfg, &n, "r");
    assert_eq!(item, serde_json::json!({"id": "x", "kind": "templated"}));
}

#[test]
fn render_batch_item_falls_back_to_default_envelope_without_template() {
    let cfg = HttpReactionConfig::default();
    let n = add_notification("q1", 7, serde_json::json!({"id": "x"}));
    let hb = build_handlebars();
    let item = render_batch_item(&hb, &cfg, &n, "r");
    // Default change-notification envelope (Pattern A item) shape.
    assert_eq!(item.get("operation"), Some(&serde_json::json!("ADD")));
    assert_eq!(item.get("queryId"), Some(&serde_json::json!("q1")));
    assert_eq!(item.get("sequenceId"), Some(&serde_json::json!(7)));
    assert_eq!(item.get("after"), Some(&serde_json::json!({"id": "x"})));
}

#[test]
fn render_batch_item_falls_back_when_template_renders_invalid_json() {
    // Template renders text that is not valid JSON → fall back to the default
    // envelope rather than producing a malformed batch item.
    let cfg = cfg_with_routes(
        None,
        HashMap::from([(
            "q1".to_string(),
            HttpQueryConfig {
                added: Some(TemplateSpec {
                    template: "not-json {{after.id}}".to_string(),
                    extension: HttpCallExt::default(),
                }),
                ..Default::default()
            },
        )]),
    );
    let n = add_notification("q1", 3, serde_json::json!({"id": "x"}));
    let hb = build_handlebars();
    let item = render_batch_item(&hb, &cfg, &n, "r");
    assert_eq!(item.get("operation"), Some(&serde_json::json!("ADD")));
    assert_eq!(item.get("after"), Some(&serde_json::json!({"id": "x"})));
}

#[test]
fn render_batch_item_resolves_default_template_and_last_segment() {
    // Default template applies when no route matches.
    let cfg = cfg_with_routes(
        Some(added_template(r#"{"d":"{{after.id}}"}"#)),
        HashMap::new(),
    );
    let n = add_notification("src.q1", 1, serde_json::json!({"id": "z"}));
    let hb = build_handlebars();
    assert_eq!(
        render_batch_item(&hb, &cfg, &n, "r"),
        serde_json::json!({"d": "z"})
    );

    // A route keyed by the last dotted segment is used for a dotted query id.
    let cfg = cfg_with_routes(
        None,
        HashMap::from([(
            "q1".to_string(),
            added_template(r#"{"seg":"{{after.id}}"}"#),
        )]),
    );
    assert_eq!(
        render_batch_item(&hb, &cfg, &n, "r"),
        serde_json::json!({"seg": "z"})
    );
}

// ---------------------------------------------------------------------------
// HttpReactionConfig::validate — base URL branch coverage
// ---------------------------------------------------------------------------

fn cfg_with_base_url(base_url: &str) -> HttpReactionConfig {
    HttpReactionConfig {
        base_url: base_url.to_string(),
        ..Default::default()
    }
}

#[test]
fn validate_accepts_http_and_https_base_urls() {
    assert!(cfg_with_base_url("http://localhost")
        .validate(&[], None)
        .is_ok());
    assert!(cfg_with_base_url("https://api.example.com/v1")
        .validate(&[], None)
        .is_ok());
}

#[test]
fn validate_rejects_non_http_scheme() {
    let err = cfg_with_base_url("ftp://example.com")
        .validate(&[], None)
        .unwrap_err();
    assert!(format!("{err:#}").contains("baseUrl"), "{err:#}");
}

#[test]
fn validate_rejects_base_url_with_query_string() {
    assert!(cfg_with_base_url("http://localhost/p?x=1")
        .validate(&[], None)
        .is_err());
}

#[test]
fn validate_rejects_base_url_with_fragment() {
    assert!(cfg_with_base_url("http://localhost/p#frag")
        .validate(&[], None)
        .is_err());
}

#[test]
fn validate_rejects_unparseable_base_url() {
    assert!(cfg_with_base_url("not a url").validate(&[], None).is_err());
}

// ---------------------------------------------------------------------------
// HttpReactionConfig::validate — batch endpoint branch coverage
// ---------------------------------------------------------------------------

fn cfg_adaptive_with_batch(endpoint: &str) -> HttpReactionConfig {
    HttpReactionConfig {
        adaptive: Some(AdaptiveBatchConfig::default()),
        batch_endpoint: Some(endpoint.to_string()),
        ..Default::default()
    }
}

#[test]
fn validate_accepts_valid_batch_endpoint() {
    assert!(cfg_adaptive_with_batch("/events/batch")
        .validate(&[], None)
        .is_ok());
}

#[test]
fn validate_rejects_empty_batch_endpoint() {
    assert!(cfg_adaptive_with_batch("").validate(&[], None).is_err());
}

#[test]
fn validate_rejects_batch_endpoint_without_leading_slash() {
    assert!(cfg_adaptive_with_batch("batch")
        .validate(&[], None)
        .is_err());
}

#[test]
fn validate_rejects_batch_endpoint_with_double_slash() {
    assert!(cfg_adaptive_with_batch("//batch")
        .validate(&[], None)
        .is_err());
}

#[test]
fn validate_rejects_batch_endpoint_absolute_url() {
    assert!(cfg_adaptive_with_batch("http://evil.example.com/batch")
        .validate(&[], None)
        .is_err());
}

#[test]
fn validate_rejects_batch_endpoint_with_template() {
    assert!(cfg_adaptive_with_batch("/batch/{{after.id}}")
        .validate(&[], None)
        .is_err());
}

// ---------------------------------------------------------------------------
// HttpReactionConfig::validate — adaptive range branch coverage
// ---------------------------------------------------------------------------

fn cfg_with_adaptive(adaptive: AdaptiveBatchConfig) -> HttpReactionConfig {
    HttpReactionConfig {
        adaptive: Some(adaptive),
        batch_endpoint: Some("/b".to_string()),
        ..Default::default()
    }
}

fn valid_adaptive() -> AdaptiveBatchConfig {
    AdaptiveBatchConfig {
        adaptive_min_batch_size: 1,
        adaptive_max_batch_size: 100,
        adaptive_window_size: 10,
        adaptive_batch_timeout_ms: 1000,
    }
}

#[test]
fn validate_rejects_zero_min_batch_size() {
    let a = AdaptiveBatchConfig {
        adaptive_min_batch_size: 0,
        ..valid_adaptive()
    };
    assert!(cfg_with_adaptive(a).validate(&[], None).is_err());
}

#[test]
fn validate_rejects_zero_max_batch_size() {
    let a = AdaptiveBatchConfig {
        adaptive_max_batch_size: 0,
        ..valid_adaptive()
    };
    assert!(cfg_with_adaptive(a).validate(&[], None).is_err());
}

#[test]
fn validate_rejects_window_size_out_of_range() {
    let zero = AdaptiveBatchConfig {
        adaptive_window_size: 0,
        ..valid_adaptive()
    };
    assert!(cfg_with_adaptive(zero).validate(&[], None).is_err());
    let too_big = AdaptiveBatchConfig {
        adaptive_window_size: 256,
        ..valid_adaptive()
    };
    assert!(cfg_with_adaptive(too_big).validate(&[], None).is_err());
}

#[test]
fn validate_accepts_window_size_boundaries() {
    for w in [1usize, 255] {
        let a = AdaptiveBatchConfig {
            adaptive_window_size: w,
            ..valid_adaptive()
        };
        assert!(
            cfg_with_adaptive(a).validate(&[], None).is_ok(),
            "window size {w} should be valid"
        );
    }
}

#[test]
fn validate_rejects_zero_batch_timeout() {
    let a = AdaptiveBatchConfig {
        adaptive_batch_timeout_ms: 0,
        ..valid_adaptive()
    };
    assert!(cfg_with_adaptive(a).validate(&[], None).is_err());
}

#[test]
fn validate_rejects_invalid_url_template_in_route() {
    let mut routes = HashMap::new();
    routes.insert(
        "q1".to_string(),
        HttpQueryConfig {
            added: Some(TemplateSpec {
                template: String::new(),
                extension: HttpCallExt {
                    url: "/x/{{#if}}".to_string(), // unclosed block → compile error
                    method: "POST".to_string(),
                    headers: HashMap::new(),
                },
            }),
            ..Default::default()
        },
    );
    let cfg = cfg_with_routes(None, routes);
    let err = cfg.validate(&["q1".to_string()], None).unwrap_err();
    assert!(format!("{err:#}").contains("url"), "{err:#}");
}

// ---------------------------------------------------------------------------
// resolve_http_url
// ---------------------------------------------------------------------------

#[test]
fn resolve_http_url_appends_relative_path() {
    assert_eq!(
        resolve_http_url("http://host:8080", "/p/q").unwrap(),
        "http://host:8080/p/q"
    );
}

#[test]
fn resolve_http_url_allows_absolute_url_matching_base() {
    assert_eq!(
        resolve_http_url("http://host:8080", "http://host:8080/p").unwrap(),
        "http://host:8080/p"
    );
}

#[test]
fn resolve_http_url_rejects_mismatched_host() {
    assert!(resolve_http_url("http://host:8080", "http://other:8080/p").is_err());
}

#[test]
fn resolve_http_url_rejects_mismatched_scheme() {
    assert!(resolve_http_url("http://host:8080", "https://host:8080/p").is_err());
}

#[test]
fn resolve_http_url_rejects_mismatched_port() {
    assert!(resolve_http_url("http://host:8080", "http://host:9090/p").is_err());
}

// ---------------------------------------------------------------------------
// Config serde: round-trips and partial adaptive defaults
// ---------------------------------------------------------------------------

#[test]
fn config_json_round_trip_preserves_all_fields() {
    let mut routes = HashMap::new();
    routes.insert("q1".to_string(), added_template(r#"{"id":"{{after.id}}"}"#));
    let cfg = HttpReactionConfig {
        base_url: "https://api.example.com".to_string(),
        token: Some("secret".to_string()),
        timeout_ms: 1234,
        output_templates: Some(HttpOutputTemplates {
            default_template: Some(added_template("d")),
            routes,
        }),
        adaptive: Some(valid_adaptive()),
        batch_endpoint: Some("/b".to_string()),
    };
    let json = serde_json::to_value(&cfg).unwrap();
    let back: HttpReactionConfig = serde_json::from_value(json).unwrap();
    assert_eq!(cfg, back);
}

#[test]
fn adaptive_partial_config_fills_remaining_defaults() {
    let json = serde_json::json!({
        "baseUrl": "http://example.com",
        "adaptive": { "adaptiveMaxBatchSize": 500 },
        "batchEndpoint": "/b"
    });
    let c: HttpReactionConfig = serde_json::from_value(json).unwrap();
    let a = c.adaptive.unwrap();
    let d = AdaptiveBatchConfig::default();
    assert_eq!(a.adaptive_max_batch_size, 500);
    assert_eq!(a.adaptive_min_batch_size, d.adaptive_min_batch_size);
    assert_eq!(a.adaptive_window_size, d.adaptive_window_size);
    assert_eq!(a.adaptive_batch_timeout_ms, d.adaptive_batch_timeout_ms);
}

// ---------------------------------------------------------------------------
// Builder API surface
// ---------------------------------------------------------------------------

#[test]
fn builder_with_config_replaces_entire_config() {
    let cfg = HttpReactionConfig {
        base_url: "https://x.example.com".to_string(),
        timeout_ms: 9999,
        ..Default::default()
    };
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_config(cfg)
        .build()
        .unwrap();
    assert_eq!(r.config.base_url, "https://x.example.com");
    assert_eq!(r.config.timeout_ms, 9999);
}

#[test]
fn builder_with_default_template_appears_in_properties() {
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_default_template(added_template(r#"{"id":"{{after.id}}"}"#))
        .build()
        .unwrap();
    let p = r.properties();
    assert!(p
        .get("outputTemplates")
        .and_then(|t| t.get("defaultTemplate"))
        .is_some());
}

#[test]
fn builder_with_recovery_policy_builds() {
    assert!(HttpReaction::builder("r")
        .with_query("q1")
        .with_recovery_policy(ReactionRecoveryPolicy::AutoSkipGap)
        .build()
        .is_ok());
}

#[test]
fn builder_with_priority_queue_capacity_builds() {
    assert!(HttpReaction::builder("r")
        .with_query("q1")
        .with_priority_queue_capacity(250)
        .build()
        .is_ok());
}

#[test]
fn auto_start_defaults_true_and_is_overridable() {
    let on = HttpReaction::builder("r").with_query("q1").build().unwrap();
    assert!(on.auto_start());
    let off = HttpReaction::builder("r")
        .with_query("q1")
        .with_auto_start(false)
        .build()
        .unwrap();
    assert!(!off.auto_start());
}

#[test]
fn token_is_included_in_properties_for_persistence() {
    let r = HttpReaction::builder("r")
        .with_query("q1")
        .with_token("sekret")
        .build()
        .unwrap();
    assert_eq!(
        r.properties().get("token"),
        Some(&serde_json::Value::String("sekret".to_string()))
    );
}

#[test]
fn type_name_is_http() {
    let r = HttpReaction::builder("r").with_query("q1").build().unwrap();
    assert_eq!(r.type_name(), "http");
}

#[test]
fn handlebars_json_helper_serializes_value() {
    let hb = build_handlebars();
    let ctx = serde_json::json!({ "after": { "id": 7, "name": "x" } });
    let out = hb
        .render_template(r#"{"row":{{json after}}}"#, &ctx)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v, serde_json::json!({"row": {"id": 7, "name": "x"}}));
}

// ---------------------------------------------------------------------------
// HttpReaction::new and start-time re-validation
// ---------------------------------------------------------------------------

#[test]
fn new_constructs_without_validating() {
    // An invalid base URL would fail build(); new() does not validate.
    let cfg = HttpReactionConfig {
        base_url: "ftp://nope".to_string(),
        ..Default::default()
    };
    let r = HttpReaction::new("r", vec!["q1".to_string()], cfg);
    assert_eq!(r.id(), "r");
    assert_eq!(r.query_ids(), vec!["q1".to_string()]);
}

#[tokio::test]
async fn start_revalidates_and_sets_error_on_invalid_config() {
    let cfg = HttpReactionConfig {
        base_url: "ftp://nope".to_string(),
        ..Default::default()
    };
    let r = HttpReaction::new("r", vec!["q1".to_string()], cfg);
    assert!(r.start().await.is_err(), "start must reject invalid config");
    assert!(matches!(r.status().await, ComponentStatus::Error));
}

// ---------------------------------------------------------------------------
// Lifecycle: idempotent stop, and full initialize → start → enqueue → stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_is_safe_without_a_prior_start() {
    let r = HttpReaction::builder("r")
        .with_base_url("http://127.0.0.1:1")
        .with_query("q1")
        .build()
        .unwrap();
    r.stop().await.unwrap();
    assert!(matches!(r.status().await, ComponentStatus::Stopped));
}

#[tokio::test]
async fn stop_is_idempotent() {
    let r = HttpReaction::builder("r")
        .with_base_url("http://127.0.0.1:1")
        .with_query("q1")
        .build()
        .unwrap();
    r.start().await.unwrap();
    r.stop().await.unwrap();
    r.stop().await.unwrap();
    assert!(matches!(r.status().await, ComponentStatus::Stopped));
}

#[tokio::test]
async fn full_lifecycle_with_runtime_context() {
    use drasi_lib::component_graph::ComponentUpdate;
    use drasi_lib::context::ReactionRuntimeContext;
    use tokio::sync::mpsc;

    let r = HttpReaction::builder("rxn")
        .with_base_url("http://127.0.0.1:1") // unreachable; delivery fails silently
        .with_query("q1")
        .build()
        .unwrap();

    let (tx, _rx) = mpsc::channel::<ComponentUpdate>(16);
    let ctx = ReactionRuntimeContext::new("inst", "rxn", None, tx, None);
    r.initialize(ctx).await;

    r.start().await.unwrap();
    assert!(matches!(r.status().await, ComponentStatus::Running));

    let qr = QueryResult::new(
        "q1".to_string(),
        1,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: serde_json::json!({"id": "x"}),
            row_signature: 0,
        }],
        HashMap::new(),
    );
    r.enqueue_query_result(qr).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    r.stop().await.unwrap();
    assert!(matches!(r.status().await, ComponentStatus::Stopped));
}

// ---------------------------------------------------------------------------
// Descriptor: ConfigValue resolution, raw-config round-trip, mappings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn descriptor_preserves_raw_config_for_lossless_properties() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "token": "raw-secret",
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
    assert_eq!(
        p.get("baseUrl"),
        Some(&serde_json::json!("http://example.com"))
    );
    assert_eq!(p.get("token"), Some(&serde_json::json!("raw-secret")));
    assert_eq!(p.get("timeoutMs"), Some(&serde_json::json!(1500)));
    assert_eq!(
        p.get("priorityQueueCapacity"),
        Some(&serde_json::json!(250))
    );
}

#[tokio::test]
async fn descriptor_resolves_env_var_config_value_with_default() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    // Unset env var with a default → resolves to the default and builds.
    let cfg = serde_json::json!({
        "baseUrl": "${DRASI_TEST_HTTP_BASE_UNSET:-https://resolved-default.example.com}"
    });
    let r = d
        .create_reaction("r", vec!["q1".to_string()], &cfg, true)
        .await;
    assert!(
        r.is_ok(),
        "env-var ConfigValue with default must resolve: {:?}",
        r.err()
    );
}

#[tokio::test]
async fn descriptor_creates_reaction_with_output_templates() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "outputTemplates": {
            "defaultTemplate": { "added": { "template": "{{after.id}}" } },
            "routes": {
                "q1": { "added": { "template": "{{after.id}}", "url": "/added", "method": "POST" } }
            }
        }
    });
    let r = d
        .create_reaction("r", vec!["q1".to_string()], &cfg, true)
        .await
        .unwrap();
    let p = r.properties();
    let templates = p.get("outputTemplates").expect("outputTemplates present");
    assert!(templates
        .get("routes")
        .and_then(|r| r.as_object())
        .map(|o| o.contains_key("q1"))
        .unwrap_or(false));
    assert!(templates.get("defaultTemplate").is_some());
}

#[tokio::test]
async fn descriptor_rejects_route_key_not_matching_a_query() {
    use drasi_plugin_sdk::ReactionPluginDescriptor;
    let d = crate::descriptor::HttpReactionDescriptor;
    let cfg = serde_json::json!({
        "baseUrl": "http://example.com",
        "outputTemplates": { "routes": { "nope": { "added": { "template": "x" } } } }
    });
    let err = d
        .create_reaction("r", vec!["q1".to_string()], &cfg, true)
        .await
        .err()
        .expect("unknown route key must be rejected");
    assert!(err.to_string().contains("nope"), "{err}");
}
