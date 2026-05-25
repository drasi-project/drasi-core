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

//! Unit tests for the unified gRPC reaction.

use super::*;
use drasi_lib::reactions::common::AdaptiveBatchConfig;
use drasi_lib::Reaction;
use serde_json::{json, Value};

#[test]
fn test_grpc_builder_defaults() {
    let reaction = GrpcReactionBuilder::new("test-reaction").build().unwrap();
    assert_eq!(reaction.id(), "test-reaction");
    let props = reaction.properties();
    assert_eq!(
        props.get("endpoint"),
        Some(&Value::String("grpc://localhost:50052".to_string()))
    );
    // Default batching is fixed with batchSize=100 — serialized via the
    // typed DTO under the `batching` key.
    let batching = props
        .get("batching")
        .expect("batching field should be present");
    assert_eq!(batching.get("mode").and_then(|v| v.as_str()), Some("fixed"));
}

#[test]
fn test_grpc_builder_custom_values() {
    let reaction = GrpcReaction::builder("test-reaction")
        .with_endpoint("grpc://api.example.com:50052")
        .with_timeout_ms(10000)
        .with_fixed_batching(200, 500)
        .with_queries(vec!["query1".to_string()])
        .build()
        .unwrap();

    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    match reaction.config().batching {
        BatchingConfig::Fixed {
            batch_size,
            batch_flush_timeout_ms,
        } => {
            assert_eq!(batch_size, 200);
            assert_eq!(batch_flush_timeout_ms, 500);
        }
        _ => panic!("expected fixed batching"),
    }
}

#[test]
fn test_grpc_new_constructor() {
    let config = GrpcReactionConfig::default();
    let reaction = GrpcReaction::new("test-reaction", vec!["query1".to_string()], config);
    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
}

#[test]
fn test_adaptive_builder() {
    let reaction = GrpcReaction::builder("adaptive-reaction")
        .with_endpoint("grpc://localhost:50053")
        .with_adaptive_batching(AdaptiveBatchConfig::default())
        .build()
        .unwrap();
    assert!(matches!(
        reaction.config().batching,
        BatchingConfig::Adaptive(_)
    ));
    let props = reaction.properties();
    let batching = props.get("batching").expect("batching present");
    assert_eq!(
        batching.get("mode").and_then(|v| v.as_str()),
        Some("adaptive")
    );
}

#[test]
fn test_config_yaml_round_trip_fixed() {
    let yaml = r#"
endpoint: "grpc://example:50052"
timeoutMs: 7500
batching:
  mode: fixed
  batchSize: 250
  batchFlushTimeoutMs: 750
"#;
    let cfg: GrpcReactionConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.endpoint, "grpc://example:50052");
    assert_eq!(cfg.timeout_ms, 7500);
    match cfg.batching {
        BatchingConfig::Fixed {
            batch_size,
            batch_flush_timeout_ms,
        } => {
            assert_eq!(batch_size, 250);
            assert_eq!(batch_flush_timeout_ms, 750);
        }
        _ => panic!("expected fixed batching"),
    }
}

#[test]
fn test_config_yaml_round_trip_adaptive() {
    let yaml = r#"
endpoint: "grpc://example:50052"
batching:
  mode: adaptive
  adaptive_min_batch_size: 25
  adaptive_max_batch_size: 750
  adaptive_window_size: 12
  adaptive_batch_timeout_ms: 1500
"#;
    let cfg: GrpcReactionConfig = serde_yaml::from_str(yaml).unwrap();
    match cfg.batching {
        BatchingConfig::Adaptive(a) => {
            assert_eq!(a.adaptive_min_batch_size, 25);
            assert_eq!(a.adaptive_max_batch_size, 750);
            assert_eq!(a.adaptive_window_size, 12);
            assert_eq!(a.adaptive_batch_timeout_ms, 1500);
        }
        _ => panic!("expected adaptive batching"),
    }
}

#[test]
fn test_dto_descriptor_round_trip() {
    use crate::descriptor::{GrpcReactionConfigDto, GrpcReactionDescriptor};
    use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;
    let descriptor = GrpcReactionDescriptor;
    // The descriptor version must reflect the breaking redesign.
    assert_eq!(descriptor.config_version(), "2.0.0");

    // Schema bundle includes our sub-DTOs.
    let schema_json = descriptor.config_schema_json();
    for name in [
        "reaction.grpc.GrpcReactionConfig",
        "reaction.grpc.BatchingConfig",
        "reaction.grpc.OutputTemplates",
        "reaction.grpc.QueryConfig",
        "reaction.grpc.TemplateSpec",
    ] {
        assert!(
            schema_json.contains(name),
            "schema missing component {name}: {schema_json}"
        );
    }

    // DTO round-trips through serde_json.
    let dto = GrpcReactionConfigDto::from(&GrpcReactionConfig::default());
    let serialized = serde_json::to_value(&dto).unwrap();
    assert_eq!(
        serialized.get("endpoint").and_then(|v| v.as_str()),
        Some("grpc://localhost:50052")
    );
}

#[tokio::test]
async fn test_descriptor_creates_reaction_with_templates() {
    use crate::descriptor::GrpcReactionDescriptor;
    use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;

    let cfg = json!({
        "endpoint": "grpc://example:50052",
        "batching": {
            "mode": "fixed",
            "batchSize": 50,
            "batchFlushTimeoutMs": 250
        },
        "outputTemplates": {
            "defaultTemplate": {
                "added": { "template": "{\"id\":\"{{after.id}}\"}" }
            },
            "routes": {
                "query-x": {
                    "deleted": { "template": "{\"removed\":\"{{before.id}}\"}" }
                }
            }
        }
    });

    let descriptor = GrpcReactionDescriptor;
    let reaction = descriptor
        .create_reaction("test-id", vec!["query-x".into()], &cfg, true)
        .await
        .expect("create_reaction succeeds");
    assert_eq!(reaction.id(), "test-id");
}
