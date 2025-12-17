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
use drasi_lib::plugin_core::Reaction;

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
