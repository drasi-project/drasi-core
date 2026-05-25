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

//! Unit tests for the unified HTTP reaction.

use std::collections::HashMap;

use drasi_lib::Reaction;

use crate::config::{
    synthesized_default_spec, AdaptiveBatchConfig, HttpCallExt, HttpOutputTemplates,
    HttpQueryConfig, HttpReactionConfig, OperationType, TemplateRouting, TemplateSpec,
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
        .with_output_templates(templates)
        .build()
        .unwrap();
    let p = r.properties();
    let ot = p
        .get("outputTemplates")
        .expect("outputTemplates should appear");
    let routes = ot
        .get("routes")
        .and_then(|v| v.as_object())
        .expect("routes object");
    assert!(routes.contains_key("q1"));
}

#[test]
fn builder_with_query_template_adds_to_routes() {
    let r = HttpReaction::builder("r")
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
fn config_deserialize_with_output_templates_and_adaptive() {
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
        },
        "adaptive": {
            "adaptive_min_batch_size": 5,
            "adaptive_max_batch_size": 500,
            "adaptive_window_size": 20,
            "adaptive_batch_timeout_ms": 250
        },
        "batchEndpoint": "/batch"
    });

    let c: HttpReactionConfig = serde_json::from_value(json).unwrap();
    assert_eq!(c.base_url, "http://example.com");
    assert!(c.adaptive.is_some());
    let a = c.adaptive.unwrap();
    assert_eq!(a.adaptive_min_batch_size, 5);
    assert_eq!(a.adaptive_max_batch_size, 500);
    assert_eq!(c.batch_endpoint.as_deref(), Some("/batch"));
    assert!(c.output_templates.is_some());
    let ot = c.output_templates.unwrap();
    assert!(ot.default_template.is_some());
    assert!(ot.routes.contains_key("q1"));
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

#[test]
fn synthesized_default_spec_uses_changes_path() {
    let spec = synthesized_default_spec("my-query");
    assert_eq!(spec.extension.url, "/changes/my-query");
    assert_eq!(spec.extension.method, "POST");
    assert!(spec.template.is_empty());
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
    assert_eq!(runtime.min_wait_time.as_millis(), 100);
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
    // Schema must serialize and include our root type plus sub-types
    let schema = d.config_schema_json();
    assert!(schema.contains("HttpReactionConfig"));
    assert!(schema.contains("HttpOutputTemplates"));
    assert!(schema.contains("HttpQueryConfig"));
    assert!(schema.contains("HttpCallSpec"));
    assert!(schema.contains("AdaptiveBatchConfig"));
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
