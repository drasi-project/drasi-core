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

use drasi_lib::Reaction;

use crate::config::{
    AdaptiveBatchConfig, HttpCallExt, HttpOutputTemplates, HttpQueryConfig, HttpReactionConfig,
    OperationType, TemplateRouting, TemplateSpec,
};
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
fn build_fails_when_adaptive_combined_with_output_templates() {
    let spec = spec_with("/x", "", HashMap::new());
    let err = HttpReaction::builder("r")
        .with_query("q1")
        .with_query_template("q1", spec)
        .with_adaptive_defaults()
        .with_batch_endpoint("/batch")
        .build()
        .err()
        .expect("build should fail");
    assert!(
        err.to_string().contains("outputTemplates"),
        "error should mention outputTemplates: {err}"
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
// resolve_call_spec resolution order
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
fn resolve_call_spec_matches_full_query_id() {
    let mut routes = HashMap::new();
    routes.insert("source.q1".to_string(), added_template("full"));
    let cfg = cfg_with_routes(None, routes);
    let s = cfg
        .resolve_call_spec("source.q1", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "full");
}

#[test]
fn resolve_call_spec_falls_back_to_last_dotted_segment() {
    let mut routes = HashMap::new();
    routes.insert("q1".to_string(), added_template("segment"));
    let cfg = cfg_with_routes(None, routes);
    // Full id "source.q1" not present; resolves via last segment "q1".
    let s = cfg
        .resolve_call_spec("source.q1", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "segment");
}

#[test]
fn resolve_call_spec_prefers_full_id_over_segment() {
    let mut routes = HashMap::new();
    routes.insert("source.q1".to_string(), added_template("full"));
    routes.insert("q1".to_string(), added_template("segment"));
    let cfg = cfg_with_routes(None, routes);
    let s = cfg
        .resolve_call_spec("source.q1", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "full");
}

#[test]
fn resolve_call_spec_falls_back_to_default_template() {
    let cfg = cfg_with_routes(Some(added_template("default")), HashMap::new());
    let s = cfg
        .resolve_call_spec("anything", OperationType::Add)
        .unwrap();
    assert_eq!(s.template, "default");
}

#[test]
fn resolve_call_spec_returns_none_without_templates() {
    let cfg = HttpReactionConfig::default();
    assert!(cfg.resolve_call_spec("q1", OperationType::Add).is_none());
}
