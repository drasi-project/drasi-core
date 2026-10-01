// Copyright 2025 The Drasi Authors.
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

use super::*;
use crate::config::SseExtension;
use drasi_lib::channels::ComponentStatus;
use drasi_lib::component_graph::ComponentUpdate;
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::{recovery::ReactionRecoveryPolicy, Reaction};
use std::time::Duration;
use tokio::net::TcpListener;

#[test]
fn test_sse_builder_defaults() {
    let reaction = SseReactionBuilder::new("test-reaction").build().unwrap();
    assert_eq!(reaction.id(), "test-reaction");
    let props = reaction.properties();
    assert_eq!(
        props.get("host"),
        Some(&serde_json::Value::String("0.0.0.0".to_string()))
    );
    assert_eq!(
        props.get("port"),
        Some(&serde_json::Value::Number(8080.into()))
    );
}

#[test]
fn test_sse_builder_custom() {
    let reaction = SseReaction::builder("test-reaction")
        .with_host("localhost")
        .with_port(9090)
        .with_sse_path("/stream")
        .with_queries(vec!["query1".to_string()])
        .build()
        .unwrap();

    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
}

#[test]
fn test_sse_new_constructor() {
    let config = SseReactionConfig::default();
    let reaction = SseReaction::new("test-reaction", vec!["query1".to_string()], config);
    assert_eq!(reaction.id(), "test-reaction");
}

#[test]
fn test_sse_builder_with_heartbeat() {
    let reaction = SseReaction::builder("test-reaction")
        .with_heartbeat_interval_ms(5000)
        .build()
        .unwrap();

    let props = reaction.properties();
    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(
        props.get("ssePath"),
        Some(&serde_json::Value::String("/events".to_string()))
    );
}

#[test]
fn test_sse_builder_with_priority_queue() {
    let reaction = SseReaction::builder("test-reaction")
        .with_priority_queue_capacity(5000)
        .build()
        .unwrap();

    assert_eq!(reaction.id(), "test-reaction");
}

#[test]
fn test_sse_builder_chaining() {
    let reaction = SseReaction::builder("chained-reaction")
        .with_host("127.0.0.1")
        .with_port(3000)
        .with_sse_path("/events/stream")
        .with_query("query1")
        .with_query("query2")
        .with_heartbeat_interval_ms(10000)
        .with_auto_start(false)
        .build()
        .unwrap();

    assert_eq!(reaction.id(), "chained-reaction");
    assert_eq!(reaction.query_ids(), vec!["query1", "query2"]);
    assert!(!reaction.auto_start());

    let props = reaction.properties();
    assert_eq!(
        props.get("host"),
        Some(&serde_json::Value::String("127.0.0.1".to_string()))
    );
    assert_eq!(
        props.get("port"),
        Some(&serde_json::Value::Number(3000.into()))
    );
    assert_eq!(
        props.get("ssePath"),
        Some(&serde_json::Value::String("/events/stream".to_string()))
    );
}

#[test]
fn test_sse_type_name() {
    let reaction = SseReaction::builder("test").build().unwrap();
    assert_eq!(reaction.type_name(), "sse");
}

#[test]
fn test_sse_builder_with_routes() {
    let query_config = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"event": "add", "data": {{json after}}}"#.to_string(),
            extension: SseExtension {
                path: Some("/custom/added".to_string()),
            },
        }),
        updated: Some(TemplateSpec {
            template: r#"{"event": "update", "before": {{json before}}, "after": {{json after}}}"#
                .to_string(),
            extension: SseExtension { path: None },
        }),
        deleted: Some(TemplateSpec {
            template: r#"{"event": "delete", "data": {{json before}}}"#.to_string(),
            extension: SseExtension {
                path: Some("/custom/deleted".to_string()),
            },
        }),
    };

    let reaction = SseReaction::builder("test-reaction")
        .with_query("query1")
        .with_route("query1", query_config)
        .build()
        .unwrap();

    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.query_ids(), vec!["query1"]);
}

#[test]
fn test_sse_config_with_routes_serialization() {
    let mut routes = std::collections::HashMap::new();
    routes.insert(
        "test-query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec {
                template: r#"{"type": "added", "data": {{json after}}}"#.to_string(),
                extension: SseExtension { path: None },
            }),
            updated: None,
            deleted: None,
        },
    );

    let config = SseReactionConfig {
        host: "0.0.0.0".to_string(),
        port: 8080,
        sse_path: "/events".to_string(),
        heartbeat_interval_ms: 30000,
        routes,
        default_template: None,
    };

    let serialized = serde_json::to_string(&config).unwrap();
    let deserialized: SseReactionConfig = serde_json::from_str(&serialized).unwrap();

    assert_eq!(config, deserialized);
}

#[test]
fn test_template_spec_creation() {
    let spec = TemplateSpec {
        template: r#"{"message": "test"}"#.to_string(),
        extension: SseExtension {
            path: Some("/custom/path".to_string()),
        },
    };

    assert_eq!(spec.extension.path, Some("/custom/path".to_string()));
    assert_eq!(spec.template, r#"{"message": "test"}"#);
}

#[test]
fn test_query_config_all_operations() {
    let config = QueryConfig {
        added: Some(TemplateSpec {
            template: "add template".to_string(),
            extension: SseExtension { path: None },
        }),
        updated: Some(TemplateSpec {
            template: "update template".to_string(),
            extension: SseExtension { path: None },
        }),
        deleted: Some(TemplateSpec {
            template: "delete template".to_string(),
            extension: SseExtension { path: None },
        }),
    };

    assert!(config.added.is_some());
    assert!(config.updated.is_some());
    assert!(config.deleted.is_some());
}

#[test]
fn test_config_default_routes_empty() {
    let config = SseReactionConfig::default();
    assert!(config.routes.is_empty());
    assert!(config.default_template.is_none());
}

#[test]
fn test_sse_builder_with_default_template() {
    let default_template = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"event": "add", "data": {{json after}}}"#.to_string(),
            extension: SseExtension { path: None },
        }),
        updated: Some(TemplateSpec {
            template: r#"{"event": "update", "data": {{json after}}}"#.to_string(),
            extension: SseExtension { path: None },
        }),
        deleted: Some(TemplateSpec {
            template: r#"{"event": "delete", "data": {{json before}}}"#.to_string(),
            extension: SseExtension { path: None },
        }),
    };

    let reaction = SseReaction::builder("test-reaction")
        .with_queries(vec!["query1".to_string(), "query2".to_string()])
        .with_default_template(default_template)
        .build()
        .unwrap();

    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.query_ids().len(), 2);
}

#[test]
fn test_sse_builder_invalid_template_fails() {
    let invalid_template = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{{invalid syntax"#.to_string(),
            extension: SseExtension { path: None },
        }),
        updated: None,
        deleted: None,
    };

    let result = SseReaction::builder("test-reaction")
        .with_query("query1")
        .with_default_template(invalid_template)
        .build();

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Invalid"));
}

#[test]
fn test_sse_builder_route_validation_passes() {
    let route_config = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"data": {{json after}}}"#.to_string(),
            extension: SseExtension { path: None },
        }),
        updated: None,
        deleted: None,
    };

    let result = SseReaction::builder("test-reaction")
        .with_query("query1")
        .with_route("query1", route_config)
        .build();

    assert!(result.is_ok());
}

#[test]
fn test_sse_builder_route_validation_fails() {
    let route_config = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"data": {{json after}}}"#.to_string(),
            extension: SseExtension { path: None },
        }),
        updated: None,
        deleted: None,
    };

    let result = SseReaction::builder("test-reaction")
        .with_query("query1")
        .with_route("query2", route_config)
        .build();

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("does not match any subscribed query"));
}

#[test]
fn test_sse_builder_route_validation_dotted_notation() {
    let route_config = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"data": {{json after}}}"#.to_string(),
            extension: SseExtension { path: None },
        }),
        updated: None,
        deleted: None,
    };

    // Should match "source.query1" with route "query1"
    let result = SseReaction::builder("test-reaction")
        .with_query("source.query1")
        .with_route("query1", route_config)
        .build();

    assert!(result.is_ok());
}

#[test]
fn test_config_with_default_template_serialization() {
    let default_template = QueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"event": "add"}"#.to_string(),
            extension: SseExtension { path: None },
        }),
        updated: None,
        deleted: None,
    };

    let config = SseReactionConfig {
        host: "0.0.0.0".to_string(),
        port: 8080,
        sse_path: "/events".to_string(),
        heartbeat_interval_ms: 30000,
        routes: std::collections::HashMap::new(),
        default_template: Some(default_template),
    };

    let serialized = serde_json::to_string(&config).unwrap();
    let deserialized: SseReactionConfig = serde_json::from_str(&serialized).unwrap();

    assert_eq!(config, deserialized);
}

#[test]
fn test_builder_fallback_produces_camel_case() {
    let reaction = SseReactionBuilder::new("sse-fallback")
        .with_host("127.0.0.1")
        .with_port(9090)
        .with_sse_path("/custom-events")
        .with_heartbeat_interval_ms(15000)
        .with_queries(vec!["q1".to_string()])
        .build()
        .unwrap();

    let props = reaction.properties();

    // Must use camelCase keys (DTO serialization)
    assert!(
        props.contains_key("ssePath"),
        "expected camelCase 'ssePath', got keys: {:?}",
        props.keys().collect::<Vec<_>>()
    );
    assert!(
        props.contains_key("heartbeatIntervalMs"),
        "expected camelCase 'heartbeatIntervalMs'"
    );

    // Must NOT have snake_case keys
    assert!(
        !props.contains_key("sse_path"),
        "should not have snake_case 'sse_path'"
    );
    assert!(
        !props.contains_key("heartbeat_interval_ms"),
        "should not have snake_case 'heartbeat_interval_ms'"
    );

    // Values should be correct
    assert_eq!(
        props.get("host").and_then(|v| v.as_str()),
        Some("127.0.0.1")
    );
    assert_eq!(props.get("port").and_then(|v| v.as_u64()), Some(9090));
    assert_eq!(
        props.get("ssePath").and_then(|v| v.as_str()),
        Some("/custom-events")
    );
    assert_eq!(
        props.get("heartbeatIntervalMs").and_then(|v| v.as_u64()),
        Some(15000)
    );
}

#[test]
fn test_recovery_trait_defaults() {
    let reaction = SseReaction::builder("test-sse")
        .with_port(0)
        .build()
        .unwrap();

    assert!(!reaction.is_durable());
    assert!(!reaction.needs_snapshot_on_fresh_start());
    assert_eq!(
        reaction.default_recovery_policy(),
        ReactionRecoveryPolicy::AutoSkipGap
    );
}

async fn sse_reaction_with_reserved_port(id: &str) -> (SseReaction, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let reaction = SseReaction::builder(id)
        .with_host("127.0.0.1")
        .with_port(listener.local_addr().unwrap().port())
        .build()
        .unwrap();
    (reaction, listener)
}

#[tokio::test]
async fn test_sse_lifecycle_bind_failure_reports_error_without_running() {
    let (reaction, occupied_listener) = sse_reaction_with_reserved_port("occupied-port").await;
    let addr = occupied_listener.local_addr().unwrap();
    let (update_tx, mut updates) = tokio::sync::mpsc::channel(16);
    reaction
        .initialize(ReactionRuntimeContext::new(
            "test-instance",
            reaction.id(),
            None,
            update_tx,
            None,
        ))
        .await;

    let error = reaction
        .start()
        .await
        .expect_err("starting on an occupied port must fail");
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert!(error.to_string().contains(&addr.to_string()));
    assert_eq!(reaction.status().await, ComponentStatus::Error);
    assert!(reaction.base.processing_task.read().await.is_none());
    assert!(reaction.base.shutdown_tx.read().await.is_none());

    let mut reported_statuses = Vec::new();
    while let Ok(ComponentUpdate::Status {
        status, message, ..
    }) = updates.try_recv()
    {
        if status == ComponentStatus::Error {
            assert!(message.unwrap().contains(&addr.to_string()));
        }
        reported_statuses.push(status);
    }
    assert_eq!(
        reported_statuses,
        vec![ComponentStatus::Starting, ComponentStatus::Error]
    );

    drop(occupied_listener);
    reaction.start().await.unwrap();
    assert_eq!(reaction.status().await, ComponentStatus::Running);
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn test_sse_lifecycle_start_binds_before_returning() {
    let (reaction, reserved_listener) = sse_reaction_with_reserved_port("ready-listener").await;
    let addr = reserved_listener.local_addr().unwrap();
    drop(reserved_listener);

    reaction.stop().await.unwrap();
    reaction.stop().await.unwrap();
    reaction.start().await.unwrap();
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    // Probe synchronously so a spawned bind cannot run before this assertion.
    let probe = std::net::TcpListener::bind(addr);
    reaction.stop().await.unwrap();
    assert_eq!(
        probe
            .expect_err("successful start must already own the configured port")
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_sse_lifecycle_stop_start_reuses_configured_port() {
    let (reaction, reserved_listener) = sse_reaction_with_reserved_port("restart-listener").await;
    let addr = reserved_listener.local_addr().unwrap();
    drop(reserved_listener);
    let client = reqwest::Client::new();

    for _ in 0..3 {
        reaction.start().await.unwrap();
        tokio::task::yield_now().await;
        let response = client
            .get(format!("http://{addr}/events"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );

        reaction.stop().await.unwrap();
        let probe = TcpListener::bind(addr)
            .await
            .expect("stop must release the listener before returning");
        drop(probe);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    reaction.stop().await.unwrap();
    reaction.stop().await.unwrap();
}

#[tokio::test]
async fn test_sse_lifecycle_repeated_start_preserves_running_server() {
    let (reaction, reserved_listener) = sse_reaction_with_reserved_port("repeated-start").await;
    let addr = reserved_listener.local_addr().unwrap();
    drop(reserved_listener);

    reaction.start().await.unwrap();
    reaction
        .start()
        .await
        .expect_err("an already started reaction must not spawn another server");
    assert_eq!(reaction.status().await, ComponentStatus::Running);
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/events"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    reaction.stop().await.unwrap();
}
