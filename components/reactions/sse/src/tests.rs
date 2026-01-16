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
use drasi_lib::Reaction;

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
        props.get("sse_path"),
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
        props.get("sse_path"),
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
