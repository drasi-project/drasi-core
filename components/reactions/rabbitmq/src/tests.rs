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
