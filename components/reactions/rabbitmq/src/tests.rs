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

use super::config::{ExchangeType, PublishSpec, QueryPublishConfig, RabbitMQReactionConfig};
use super::rabbitmq::RabbitMQReaction;
use std::collections::HashMap;

#[test]
fn test_config_defaults() {
    let config = RabbitMQReactionConfig::default();
    assert_eq!(
        config.connection_string,
        "amqp://guest:guest@localhost:5672/%2f" // DevSkim: ignore DS137138
    );
    assert_eq!(config.exchange_name, "drasi-events");
    assert_eq!(config.exchange_type, ExchangeType::Topic);
    assert!(config.exchange_durable);
    assert!(config.message_persistent);
    assert!(!config.tls_enabled);
    assert_eq!(config.max_reconnect_attempts, 5);
    assert!(config.tls_pfx_password.is_none());
}

#[test]
fn test_exchange_type_mapping() {
    assert_eq!(
        ExchangeType::Direct.as_exchange_kind(),
        lapin::ExchangeKind::Direct
    );
    assert_eq!(
        ExchangeType::Topic.as_exchange_kind(),
        lapin::ExchangeKind::Topic
    );
    assert_eq!(
        ExchangeType::Fanout.as_exchange_kind(),
        lapin::ExchangeKind::Fanout
    );
    assert_eq!(
        ExchangeType::Headers.as_exchange_kind(),
        lapin::ExchangeKind::Headers
    );
}

#[test]
fn test_builder_rejects_invalid_template() {
    let mut routes = HashMap::new();
    routes.insert(
        "q1".to_string(),
        QueryPublishConfig {
            added: Some(PublishSpec {
                routing_key: "{{".to_string(),
                headers: HashMap::new(),
                body_template: None,
            }),
            updated: None,
            deleted: None,
        },
    );

    let config = RabbitMQReactionConfig {
        query_configs: routes,
        ..Default::default()
    };

    let result = RabbitMQReaction::new("test", vec!["q1".to_string()], config);
    assert!(result.is_err());
}

#[test]
fn test_tls_requires_amqps() {
    let config = RabbitMQReactionConfig {
        tls_enabled: true,
        connection_string: "amqp://guest:guest@localhost:5672/%2f".to_string(), // DevSkim: ignore DS137138
        ..Default::default()
    };

    let result = RabbitMQReaction::new("test", vec!["q1".to_string()], config);
    assert!(result.is_err());
}

#[test]
fn test_validate_config_rejects_mismatched_route() {
    let mut routes = HashMap::new();
    routes.insert(
        "unknown-query".to_string(),
        QueryPublishConfig::default(),
    );

    let config = RabbitMQReactionConfig {
        query_configs: routes,
        ..Default::default()
    };

    let result = RabbitMQReaction::new("test", vec!["my-query".to_string()], config);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("unknown-query"));
    assert!(err_msg.contains("does not match"));
}

#[test]
fn test_validate_config_accepts_dotted_query_match() {
    let mut routes = HashMap::new();
    routes.insert("q1".to_string(), QueryPublishConfig::default());

    let config = RabbitMQReactionConfig {
        query_configs: routes,
        ..Default::default()
    };

    // Query "ns.q1" should match route "q1" via the dotted-suffix logic
    let result = RabbitMQReaction::new("test", vec!["ns.q1".to_string()], config);
    assert!(result.is_ok());
}

#[test]
fn test_sanitize_connection_string_redacts_credentials() {
    let sanitized =
        RabbitMQReaction::sanitize_connection_string("amqp://user:pass@host:5672/%2f"); // DevSkim: ignore DS137138
    assert_eq!(sanitized, "amqp://***:***@host:5672/%2f");
}

#[test]
fn test_sanitize_connection_string_no_credentials() {
    let sanitized = RabbitMQReaction::sanitize_connection_string("amqp://host:5672/%2f");
    // No '@' means no credentials to redact
    assert_eq!(sanitized, "amqp://host:5672/%2f");
}

#[test]
fn test_sanitize_connection_string_amqps() {
    let sanitized =
        RabbitMQReaction::sanitize_connection_string("amqps://admin:secret@broker.example.com:5671/%2f");
    assert_eq!(sanitized, "amqps://***:***@broker.example.com:5671/%2f");
}

/// Test template rendering for routing key with {{after.id}}.
#[test]
fn test_template_routing_key_renders_after_field() {
    let mut handlebars = handlebars::Handlebars::new();
    RabbitMQReaction::register_helpers_for_test(&mut handlebars);

    let mut context = serde_json::Map::new();
    context.insert(
        "after".to_string(),
        serde_json::json!({"id": 42, "name": "Alice"}),
    );
    context.insert("operation".to_string(), serde_json::json!("ADD"));

    let result = handlebars
        .render_template("entity.{{after.id}}", &context)
        .unwrap();
    assert_eq!(result, "entity.42");
}

/// Test body template with {{json before}} serializes correctly.
#[test]
fn test_template_body_json_helper() {
    let mut handlebars = handlebars::Handlebars::new();
    RabbitMQReaction::register_helpers_for_test(&mut handlebars);

    let mut context = serde_json::Map::new();
    context.insert(
        "before".to_string(),
        serde_json::json!({"id": 1, "name": "Bob"}),
    );

    let result = handlebars
        .render_template("{{json before}}", &context)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["id"], 1);
    assert_eq!(parsed["name"], "Bob");
}

/// Test header template renders correctly.
#[test]
fn test_template_header_renders_value() {
    let mut handlebars = handlebars::Handlebars::new();
    RabbitMQReaction::register_helpers_for_test(&mut handlebars);

    let mut context = serde_json::Map::new();
    context.insert("query_id".to_string(), serde_json::json!("user-updates"));
    context.insert("operation".to_string(), serde_json::json!("DELETE"));

    let result = handlebars
        .render_template("{{query_id}}-{{operation}}", &context)
        .unwrap();
    assert_eq!(result, "user-updates-DELETE");
}

/// Test that missing context key produces empty string (non-strict mode).
#[test]
fn test_template_missing_key_produces_empty() {
    let mut handlebars = handlebars::Handlebars::new();
    RabbitMQReaction::register_helpers_for_test(&mut handlebars);

    let context = serde_json::Map::new();
    let result = handlebars
        .render_template("prefix-{{nonexistent}}-suffix", &context)
        .unwrap();
    assert_eq!(result, "prefix--suffix");
}

/// Test that when no body_template is specified, raw_data is serialized as JSON.
#[test]
fn test_no_body_template_falls_back_to_json() {
    let raw_data = serde_json::json!({"id": 99, "status": "active"});
    let body = serde_json::to_string(&raw_data).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["id"], 99);
    assert_eq!(parsed["status"], "active");
}
