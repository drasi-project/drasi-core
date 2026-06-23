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

//! Unit tests for the gRPC reaction.

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
fn test_grpc_recovery_archetype_defaults() {
    use drasi_lib::recovery::ReactionRecoveryPolicy;
    let reaction = GrpcReactionBuilder::new("test-reaction").build().unwrap();
    // gRPC is a stateless at-least-once trigger reaction (archetype 2a).
    assert!(!reaction.is_durable());
    assert!(!reaction.needs_snapshot_on_fresh_start());
    assert_eq!(
        reaction.default_recovery_policy(),
        ReactionRecoveryPolicy::Strict
    );
}

#[test]
fn test_grpc_builder_recovery_policy_override() {
    use drasi_lib::recovery::ReactionRecoveryPolicy;
    let reaction = GrpcReactionBuilder::new("test-reaction")
        .with_recovery_policy(ReactionRecoveryPolicy::AutoSkipGap)
        .build()
        .unwrap();
    assert_eq!(
        reaction.base.recovery_policy,
        Some(ReactionRecoveryPolicy::AutoSkipGap)
    );
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
    let reaction = GrpcReaction::new("test-reaction", vec!["query1".to_string()], config)
        .expect("valid config");
    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
}

#[tokio::test]
async fn test_grpc_trait_surface() {
    use drasi_lib::channels::ComponentStatus;

    let reaction = GrpcReaction::builder("surface")
        .from_query("q1")
        .build()
        .unwrap();
    assert_eq!(reaction.type_name(), "grpc");
    // auto_start defaults to true.
    assert!(reaction.auto_start());
    // A freshly built reaction is Stopped until the runtime starts it.
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);

    // auto_start is controllable through the builder.
    let no_auto = GrpcReaction::builder("surface-no-auto")
        .from_query("q1")
        .with_auto_start(false)
        .build()
        .unwrap();
    assert!(!no_auto.auto_start());
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
        BatchingConfig::Adaptive { .. }
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
  adaptiveMinBatchSize: 25
  adaptiveMaxBatchSize: 750
  adaptiveWindowSize: 12
  adaptiveBatchTimeoutMs: 1500
"#;
    let cfg: GrpcReactionConfig = serde_yaml::from_str(yaml).unwrap();
    match cfg.batching {
        BatchingConfig::Adaptive {
            adaptive_min_batch_size,
            adaptive_max_batch_size,
            adaptive_window_size,
            adaptive_batch_timeout_ms,
        } => {
            assert_eq!(adaptive_min_batch_size, 25);
            assert_eq!(adaptive_max_batch_size, 750);
            assert_eq!(adaptive_window_size, 12);
            assert_eq!(adaptive_batch_timeout_ms, 1500);
        }
        _ => panic!("expected adaptive batching"),
    }
}

#[test]
fn test_config_adaptive_serializes_camel_case() {
    // The runtime config must serialize adaptive keys in camelCase, consistent
    // with the descriptor DTO and the rest of the gRPC reaction config.
    let cfg = GrpcReactionConfig {
        batching: BatchingConfig::adaptive(AdaptiveBatchConfig {
            adaptive_min_batch_size: 5,
            adaptive_max_batch_size: 50,
            adaptive_window_size: 8,
            adaptive_batch_timeout_ms: 200,
        }),
        ..Default::default()
    };
    let value = serde_json::to_value(&cfg).unwrap();
    let batching = value.get("batching").expect("batching present");
    assert_eq!(
        batching.get("mode").and_then(|v| v.as_str()),
        Some("adaptive")
    );
    assert_eq!(
        batching
            .get("adaptiveMinBatchSize")
            .and_then(|v| v.as_u64()),
        Some(5)
    );
    assert_eq!(
        batching
            .get("adaptiveMaxBatchSize")
            .and_then(|v| v.as_u64()),
        Some(50)
    );
    assert_eq!(
        batching.get("adaptiveWindowSize").and_then(|v| v.as_u64()),
        Some(8)
    );
    assert_eq!(
        batching
            .get("adaptiveBatchTimeoutMs")
            .and_then(|v| v.as_u64()),
        Some(200)
    );
    assert!(batching.get("adaptive_min_batch_size").is_none());
}

#[test]
fn test_dto_descriptor_round_trip() {
    use crate::descriptor::{GrpcReactionConfigDto, GrpcReactionDescriptor};
    use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;
    let descriptor = GrpcReactionDescriptor;
    // The descriptor version must reflect the proto v3 redesign.
    assert_eq!(descriptor.config_version(), "3.0.0");

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

#[test]
fn test_from_query_alias_appends_queries() {
    let reaction = GrpcReaction::builder("t")
        .from_query("q1")
        .from_query("q2")
        .build()
        .unwrap();
    assert_eq!(
        reaction.query_ids(),
        vec!["q1".to_string(), "q2".to_string()]
    );
}

#[test]
fn test_builder_rejects_invalid_template_at_construction() {
    use crate::config::OutputTemplates;
    use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};

    let templates = OutputTemplates {
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::with_extension(
                "{{",
                crate::config::GrpcTemplateExtension::default(),
            )),
            ..Default::default()
        }),
        routes: std::collections::HashMap::new(),
    };
    let err = match GrpcReaction::builder("t")
        .with_query("q1")
        .with_output_templates(templates)
        .build()
    {
        Ok(_) => panic!("expected build to reject the invalid template"),
        Err(e) => e,
    };
    assert!(
        format!("{err:#}").contains("template"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn test_builder_rejects_unknown_route_key_at_construction() {
    use crate::config::OutputTemplates;
    use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};

    let mut routes = std::collections::HashMap::new();
    routes.insert(
        "not-subscribed".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::with_extension(
                r#"{"id":"{{after.id}}"}"#,
                crate::config::GrpcTemplateExtension::default(),
            )),
            ..Default::default()
        },
    );
    let templates = OutputTemplates {
        default_template: None,
        routes,
    };
    let err = match GrpcReaction::builder("t")
        .with_query("q1")
        .with_output_templates(templates)
        .build()
    {
        Ok(_) => panic!("expected build to reject the unknown route key"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("not-subscribed"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_dto_metadata_accepts_static_and_env_config_values() {
    use crate::descriptor::GrpcReactionConfigDto;

    let cfg = json!({
        "endpoint": "grpc://example:50052",
        "metadata": {
            "x-tenant": "tenant-9",
            "authorization": { "kind": "EnvironmentVariable", "name": "SOME_TOKEN_ENV" }
        }
    });
    let dto: GrpcReactionConfigDto = serde_json::from_value(cfg).unwrap();
    assert_eq!(
        dto.metadata.len(),
        2,
        "both metadata forms must deserialize"
    );
    assert!(dto.metadata.contains_key("authorization"));
}

#[test]
fn test_builder_sets_connection_and_retry_fields() {
    let reaction = GrpcReaction::builder("conn")
        .with_endpoint("grpc://host:9")
        .with_timeout_ms(1111)
        .with_max_retries(7)
        .with_connection_retry_attempts(4)
        .with_initial_connection_timeout_ms(2222)
        .build()
        .unwrap();
    let cfg = reaction.config();
    assert_eq!(cfg.endpoint, "grpc://host:9");
    assert_eq!(cfg.timeout_ms, 1111);
    assert_eq!(cfg.max_retries, 7);
    assert_eq!(cfg.connection_retry_attempts, 4);
    assert_eq!(cfg.initial_connection_timeout_ms, 2222);
}

#[test]
fn test_builder_metadata_accumulates_then_replaces() {
    let reaction = GrpcReaction::builder("meta")
        .with_metadata("a", "1")
        .with_metadata("b", "2")
        .build()
        .unwrap();
    assert_eq!(reaction.config().metadata.len(), 2);

    let mut replacement = std::collections::HashMap::new();
    replacement.insert("only".to_string(), "x".to_string());
    let reaction = GrpcReaction::builder("meta2")
        .with_metadata("dropped", "y")
        .with_all_metadata(replacement)
        .build()
        .unwrap();
    assert_eq!(reaction.config().metadata.len(), 1);
    assert_eq!(
        reaction.config().metadata.get("only").map(String::as_str),
        Some("x")
    );
    assert!(
        !reaction.config().metadata.contains_key("dropped"),
        "with_all_metadata replaces the whole map"
    );
}

#[test]
fn test_builder_sets_output_format() {
    let reaction = GrpcReaction::builder("of")
        .with_output_format(OutputFormat::Proto)
        .build()
        .unwrap();
    assert_eq!(reaction.config().output_format, OutputFormat::Proto);
}

#[test]
fn test_builder_min_batch_size_switches_fixed_to_adaptive() {
    // Documented side-effect: calling an adaptive setter on a builder that
    // still holds the default Fixed config flips it to Adaptive, taking
    // defaults for the unset adaptive fields.
    let reaction = GrpcReaction::builder("adapt-side")
        .with_min_batch_size(5)
        .build()
        .unwrap();
    match reaction.config().batching {
        BatchingConfig::Adaptive {
            adaptive_min_batch_size,
            adaptive_max_batch_size,
            adaptive_window_size,
            adaptive_batch_timeout_ms,
        } => {
            assert_eq!(adaptive_min_batch_size, 5);
            assert_eq!(adaptive_max_batch_size, 100, "default max preserved");
            assert_eq!(adaptive_window_size, 10, "default window preserved");
            assert_eq!(adaptive_batch_timeout_ms, 1000, "default timeout preserved");
        }
        _ => panic!("with_min_batch_size must enable adaptive mode"),
    }
}

#[test]
fn test_builder_adaptive_setters_accumulate_on_same_config() {
    let reaction = GrpcReaction::builder("adapt-chain")
        .with_min_batch_size(5)
        .with_max_batch_size(50)
        .with_window_size(12)
        .with_batch_timeout_ms(250)
        .build()
        .unwrap();
    match reaction.config().batching {
        BatchingConfig::Adaptive {
            adaptive_min_batch_size,
            adaptive_max_batch_size,
            adaptive_window_size,
            adaptive_batch_timeout_ms,
        } => {
            assert_eq!(adaptive_min_batch_size, 5);
            assert_eq!(adaptive_max_batch_size, 50);
            assert_eq!(adaptive_window_size, 12);
            assert_eq!(adaptive_batch_timeout_ms, 250);
        }
        _ => panic!("expected adaptive batching"),
    }
}

#[test]
fn test_builder_with_adaptive_defaults() {
    let reaction = GrpcReaction::builder("adapt-def")
        .with_adaptive_defaults()
        .build()
        .unwrap();
    assert!(matches!(
        reaction.config().batching,
        BatchingConfig::Adaptive { .. }
    ));
}

#[test]
fn test_builder_with_config_replaces_entire_config() {
    let custom = GrpcReactionConfig {
        endpoint: "grpc://replaced:1".to_string(),
        timeout_ms: 4242,
        ..Default::default()
    };
    let reaction = GrpcReaction::builder("cfg")
        .with_endpoint("grpc://will-be-overwritten:2")
        .with_config(custom)
        .build()
        .unwrap();
    assert_eq!(reaction.config().endpoint, "grpc://replaced:1");
    assert_eq!(reaction.config().timeout_ms, 4242);
}

#[test]
fn test_builder_with_fixed_batching_helper() {
    let reaction = GrpcReaction::builder("fixed")
        .with_fixed_batching(33, 44)
        .build()
        .unwrap();
    assert_eq!(
        reaction.config().batching,
        BatchingConfig::Fixed {
            batch_size: 33,
            batch_flush_timeout_ms: 44
        }
    );
}

#[tokio::test]
async fn test_builder_with_priority_queue_capacity_builds() {
    let reaction = GrpcReaction::builder("pq")
        .from_query("q1")
        .with_priority_queue_capacity(256)
        .build()
        .unwrap();
    assert_eq!(reaction.id(), "pq");
    assert_eq!(
        reaction.status().await,
        drasi_lib::channels::ComponentStatus::Stopped
    );
}

#[test]
fn test_with_priority_queue_capacity_constructor() {
    let reaction = GrpcReaction::with_priority_queue_capacity(
        "pqc",
        vec!["q1".to_string()],
        GrpcReactionConfig::default(),
        128,
    )
    .expect("valid config");
    assert_eq!(reaction.id(), "pqc");
    assert_eq!(reaction.query_ids(), vec!["q1".to_string()]);
}

#[test]
fn test_new_constructor_rejects_invalid_endpoint() {
    let config = GrpcReactionConfig {
        endpoint: "not a uri".to_string(),
        ..Default::default()
    };
    let err = match GrpcReaction::new("bad", vec![], config) {
        Ok(_) => panic!("expected new() to reject the invalid endpoint"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("invalid endpoint"), "got: {err}");
}

#[test]
fn test_builder_rejects_invalid_endpoint_at_build() {
    let err = match GrpcReaction::builder("bad")
        .with_endpoint("not a uri")
        .build()
    {
        Ok(_) => panic!("expected build() to reject the invalid endpoint"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("invalid endpoint"), "got: {err}");
}

/// Send-path and runner behavioral tests that exercise the gRPC client/runner
/// loops against an in-process mock `ReactionService` server.
mod integration {
    use super::*;
    use crate::config::BatchingConfig;
    use crate::connection::create_client;
    use crate::proto::ProtoQueryResultItem;
    use crate::runner_adaptive::{self, AdaptiveRunnerParams};
    use crate::runner_fixed::{self, FixedRunnerParams};
    use crate::send::send_batch_with_retry;
    use crate::test_server;
    use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
    use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
    use drasi_lib::reactions::common::AdaptiveBatchConfig;
    use std::collections::HashMap;
    use std::time::Duration;

    fn item() -> ProtoQueryResultItem {
        use crate::proto::drasi_v1::QueryResultItemType;
        ProtoQueryResultItem {
            item_type: QueryResultItemType::Add as i32,
            row_signature: 0,
            before: None,
            after: None,
            sequence: 0,
            timestamp: None,
            metadata: None,
            payload: None,
        }
    }

    fn config_for(endpoint: String, batching: BatchingConfig) -> GrpcReactionConfig {
        GrpcReactionConfig {
            endpoint,
            timeout_ms: 2000,
            max_retries: 3,
            connection_retry_attempts: 3,
            initial_connection_timeout_ms: 1000,
            batching,
            ..GrpcReactionConfig::default()
        }
    }

    fn query_result(query_id: &str, count: usize) -> QueryResult {
        let results = (0..count)
            .map(|i| ResultDiff::Add {
                data: serde_json::json!({ "id": i }),
                row_signature: 0,
            })
            .collect();
        QueryResult::new(
            query_id.to_string(),
            1,
            chrono::Utc::now(),
            results,
            HashMap::new(),
        )
    }

    // ---- send_batch_with_retry (#19) ------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_succeeds_against_healthy_server() {
        let server = test_server::start().await;
        let mut client = create_client(&server.endpoint, 2000).await.unwrap();
        let result = send_batch_with_retry(
            &mut client,
            vec![item(), item()],
            "q1",
            &HashMap::new(),
            0,
            &server.endpoint,
            2000,
        )
        .await
        .unwrap();
        assert!(!result.0, "healthy send should not request a new client");
        assert!(result.1.is_none());
        assert_eq!(server.recorder.total_items().await, 2);
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_retries_transient_server_failure_then_succeeds() {
        // Server rejects the first request, accepts the second.
        let server = test_server::start_with_failures(1).await;
        let mut client = create_client(&server.endpoint, 2000).await.unwrap();
        let result = send_batch_with_retry(
            &mut client,
            vec![item()],
            "q1",
            &HashMap::new(),
            3,
            &server.endpoint,
            2000,
        )
        .await
        .unwrap();
        assert!(!result.0);
        assert_eq!(server.recorder.total_items().await, 1);
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_returns_error_when_failures_exceed_max_retries() {
        // Server always rejects; max_retries=0 means the first failure is fatal.
        let server = test_server::start_with_failures(100).await;
        let mut client = create_client(&server.endpoint, 2000).await.unwrap();
        let result = send_batch_with_retry(
            &mut client,
            vec![item()],
            "q1",
            &HashMap::new(),
            0,
            &server.endpoint,
            2000,
        )
        .await;
        assert!(result.is_err(), "exhausted retries should surface an error");
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_propagates_metadata_as_grpc_headers() {
        // The configured `metadata` map must be set as actual gRPC request
        // headers (in addition to the in-body field) so receivers can read
        // auth/routing entries via tonic::Request::metadata() — matching
        // the README's "authentication or routing headers" framing.
        let server = test_server::start().await;
        let mut client = create_client(&server.endpoint, 2000).await.unwrap();

        let mut metadata = HashMap::new();
        metadata.insert("authorization".to_string(), "Bearer abc123".to_string());
        metadata.insert("x-tenant".to_string(), "tenant-42".to_string());

        send_batch_with_retry(
            &mut client,
            vec![item()],
            "q1",
            &metadata,
            0,
            &server.endpoint,
            2000,
        )
        .await
        .unwrap();

        let batches = server.recorder.batches().await;
        assert_eq!(batches.len(), 1);
        let observed = &batches[0].metadata_headers;
        assert_eq!(
            observed.get("authorization").map(String::as_str),
            Some("Bearer abc123"),
            "authorization must arrive as a gRPC header (not only in the body field)"
        );
        assert_eq!(
            observed.get("x-tenant").map(String::as_str),
            Some("tenant-42")
        );
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_skips_metadata_entries_with_invalid_header_names() {
        // Invalid header names (e.g. containing uppercase or whitespace)
        // must be logged-and-skipped rather than failing the batch — the
        // entry still rides in the body field.
        let server = test_server::start().await;
        let mut client = create_client(&server.endpoint, 2000).await.unwrap();

        let mut metadata = HashMap::new();
        metadata.insert("Invalid Header Name".to_string(), "v".to_string());
        metadata.insert("x-good".to_string(), "ok".to_string());

        send_batch_with_retry(
            &mut client,
            vec![item()],
            "q1",
            &metadata,
            0,
            &server.endpoint,
            2000,
        )
        .await
        .unwrap();

        let batches = server.recorder.batches().await;
        assert_eq!(batches.len(), 1, "batch must still be delivered");
        let observed = &batches[0].metadata_headers;
        assert_eq!(observed.get("x-good").map(String::as_str), Some("ok"));
        assert!(
            !observed.keys().any(|k| k.contains(' ')),
            "invalid header name must not be propagated"
        );
        server.shutdown().await;
    }

    // ---- fixed runner (#20) ---------------------------------------------

    fn running_base() -> ReactionBase {
        ReactionBase::new(ReactionBaseParams::new("test-grpc", vec!["q1".to_string()]))
    }

    /// A base wired to an in-memory state store (via the runtime context) and
    /// set Running, so tests can pre-seed and inspect persisted checkpoints with
    /// `base.read_checkpoint` / `base.write_checkpoint`.
    async fn running_base_with_store() -> ReactionBase {
        let base = running_base();
        let store = std::sync::Arc::new(drasi_lib::MemoryStateStoreProvider::new());
        let (graph, _rx) = drasi_lib::component_graph::ComponentGraph::new("test-instance");
        let context = drasi_lib::context::ReactionRuntimeContext::new(
            "test-instance",
            "test-grpc",
            Some(store),
            graph.update_sender(),
            None,
        );
        base.initialize(context).await;
        set_running(&base).await;
        base
    }

    /// Build a `QueryResult` with an explicit per-query sequence and `count`
    /// Add diffs.
    fn query_result_seq(query_id: &str, count: usize, sequence: u64) -> QueryResult {
        let results = (0..count)
            .map(|i| ResultDiff::Add {
                data: serde_json::json!({ "id": i }),
                row_signature: 0,
            })
            .collect();
        QueryResult::new(
            query_id.to_string(),
            sequence,
            chrono::Utc::now(),
            results,
            HashMap::new(),
        )
    }

    async fn set_running(base: &ReactionBase) {
        base.set_status(ComponentStatus::Running, None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_persists_checkpoint_after_delivery() {
        let server = test_server::start().await;
        let base = running_base_with_store().await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 2,
                batch_flush_timeout_ms: 10_000,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 2,
            batch_flush_timeout_ms: 10_000,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        // A result at sequence 7 with two diffs flushes one batch.
        base.enqueue_query_result(query_result_seq("q1", 2, 7))
            .await
            .unwrap();

        let total = server
            .recorder
            .wait_for_items(2, Duration::from_secs(5))
            .await;
        assert_eq!(total, 2);

        // The checkpoint should advance to the delivered sequence.
        let cp = wait_for_checkpoint(&base, "q1", 7, Duration::from_secs(5)).await;
        assert_eq!(cp, Some(7), "checkpoint must persist the acked sequence");

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_does_not_advance_checkpoint_for_split_result_tail() {
        // A single QueryResult whose diffs span two batches must not advance the
        // checkpoint until the batch containing its terminal item is acked.
        let server = test_server::start().await;
        let base = running_base_with_store().await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        // batch_size=2 with a 3-diff result forces a split: [d0,d1] then [d2].
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 2,
                batch_flush_timeout_ms: 10_000,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 2,
            batch_flush_timeout_ms: 10_000,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        base.enqueue_query_result(query_result_seq("q1", 3, 9))
            .await
            .unwrap();

        // All three items are delivered (across two batches).
        let total = server
            .recorder
            .wait_for_items(3, Duration::from_secs(5))
            .await;
        assert_eq!(total, 3);

        // Only after the terminal item is acked does the checkpoint reach 9 —
        // never a partial/over-advanced value.
        let cp = wait_for_checkpoint(&base, "q1", 9, Duration::from_secs(5)).await;
        assert_eq!(cp, Some(9));

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    /// Poll the persisted checkpoint until it reaches `expected` or times out.
    async fn wait_for_checkpoint(
        base: &ReactionBase,
        query_id: &str,
        expected: u64,
        timeout: Duration,
    ) -> Option<u64> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let cp = base
                .read_checkpoint(query_id)
                .await
                .ok()
                .flatten()
                .map(|c| c.sequence);
            if cp == Some(expected) || tokio::time::Instant::now() >= deadline {
                return cp;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Poll the reaction status until it reaches `target` or the timeout elapses.
    async fn wait_for_status(
        base: &ReactionBase,
        target: ComponentStatus,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if base.get_status().await == target {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_flushes_on_batch_size() {
        let server = test_server::start().await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 2,
                batch_flush_timeout_ms: 10_000,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 2,
            batch_flush_timeout_ms: 10_000,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        base.enqueue_query_result(query_result("q1", 2))
            .await
            .unwrap();

        let total = server
            .recorder
            .wait_for_items(2, Duration::from_secs(5))
            .await;
        assert_eq!(total, 2, "batch of size 2 should flush immediately");

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_flushes_on_timeout() {
        let server = test_server::start().await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 100,
                batch_flush_timeout_ms: 100,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 100,
            batch_flush_timeout_ms: 100,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        // One item, well below batch_size — only the flush timer can deliver it.
        base.enqueue_query_result(query_result("q1", 1))
            .await
            .unwrap();

        let total = server
            .recorder
            .wait_for_items(1, Duration::from_secs(5))
            .await;
        assert_eq!(total, 1, "partial batch should flush on timeout");

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_flushes_previous_query_on_query_change() {
        let server = test_server::start().await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 100,
                batch_flush_timeout_ms: 10_000,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 100,
            batch_flush_timeout_ms: 10_000,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        base.enqueue_query_result(query_result("query-a", 3))
            .await
            .unwrap();
        base.enqueue_query_result(query_result("query-b", 1))
            .await
            .unwrap();

        // Dequeuing query-b flushes the buffered query-a batch deterministically.
        let total = server
            .recorder
            .wait_for_items(3, Duration::from_secs(5))
            .await;
        assert_eq!(total, 3);
        let batches = server.recorder.batches().await;
        assert_eq!(batches[0].query_id, "query-a");
        assert_eq!(batches[0].item_count, 3);

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_skips_batch_and_stays_running_under_auto_skip_gap() {
        // Under the AutoSkipGap recovery policy, a delivery that exhausts its
        // retry budget drops the batch and the reaction MUST stay Running rather
        // than going to Error. The server rejects the first 4 requests — exactly
        // what the first batch consumes with max_retries=3 (4 attempts) — then
        // accepts, so a later batch is still delivered, proving the reaction
        // kept running. (Under the default Strict policy it would fail-stop;
        // see `fixed_runner_fail_stops_on_delivery_failure_under_strict`.)
        let server = test_server::start_with_failures(4).await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 1,
                batch_flush_timeout_ms: 10_000,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 1,
            batch_flush_timeout_ms: 10_000,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::AutoSkipGap,
        }));

        // First batch: the server rejects every retry, so the batch is dropped.
        base.enqueue_query_result(query_result("q1", 1))
            .await
            .unwrap();
        // Wait past the retry/backoff window so the failed flush resolves.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            base.get_status().await,
            ComponentStatus::Running,
            "reaction must stay Running after a skipped batch under AutoSkipGap"
        );

        // Later batch: the server now accepts, so it is delivered — proof the
        // reaction kept running after dropping the first batch.
        base.enqueue_query_result(query_result("q1", 2))
            .await
            .unwrap();
        let total = server
            .recorder
            .wait_for_items(1, Duration::from_secs(5))
            .await;
        assert_eq!(total, 1, "a later batch is delivered after continuing");
        assert_eq!(base.get_status().await, ComponentStatus::Running);

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fixed_runner_fail_stops_on_delivery_failure_under_strict() {
        // Under the default Strict policy, a delivery that exhausts its retry
        // budget must set the reaction to Error (fail-stop) without advancing the
        // checkpoint, so the batch replays from the outbox on restart.
        let server = test_server::start_with_failures(100).await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::Fixed {
                batch_size: 1,
                batch_flush_timeout_ms: 10_000,
            },
        );
        let handle = tokio::spawn(runner_fixed::run(FixedRunnerParams {
            reaction_name: "test-grpc".to_string(),
            batch_size: 1,
            batch_flush_timeout_ms: 10_000,
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        base.enqueue_query_result(query_result("q1", 1))
            .await
            .unwrap();

        // The reaction should transition to Error within the retry/backoff window.
        let errored = wait_for_status(&base, ComponentStatus::Error, Duration::from_secs(5)).await;
        assert!(
            errored,
            "reaction must fail-stop (Error) on sustained delivery failure under Strict"
        );

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    // ---- adaptive runner (#21) ------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adaptive_runner_forwards_large_single_query_batch() {
        let server = test_server::start().await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::adaptive(AdaptiveBatchConfig::default()),
        );
        let handle = tokio::spawn(runner_adaptive::run(AdaptiveRunnerParams {
            reaction_name: "test-grpc".to_string(),
            adaptive: AdaptiveBatchConfig::default(),
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        // >= 100 items for one query triggers the early forward path.
        base.enqueue_query_result(query_result("q1", 100))
            .await
            .unwrap();

        let total = server
            .recorder
            .wait_for_items(100, Duration::from_secs(5))
            .await;
        assert_eq!(total, 100);

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adaptive_runner_forwards_previous_query_on_query_change() {
        let server = test_server::start().await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::adaptive(AdaptiveBatchConfig::default()),
        );
        let handle = tokio::spawn(runner_adaptive::run(AdaptiveRunnerParams {
            reaction_name: "test-grpc".to_string(),
            adaptive: AdaptiveBatchConfig::default(),
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        base.enqueue_query_result(query_result("query-a", 3))
            .await
            .unwrap();
        base.enqueue_query_result(query_result("query-b", 1))
            .await
            .unwrap();

        let total = server
            .recorder
            .wait_for_items(3, Duration::from_secs(5))
            .await;
        assert!(
            total >= 3,
            "query-a batch should be forwarded on query change"
        );

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adaptive_runner_skips_batch_and_stays_running_under_auto_skip_gap() {
        // Under the AutoSkipGap recovery policy, a delivery that exhausts its
        // retry budget drops the batch and the reaction stays Running. The server
        // rejects the first 4 requests (what one batch consumes with
        // max_retries=3) then accepts.
        let server = test_server::start_with_failures(4).await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::adaptive(AdaptiveBatchConfig::default()),
        );
        let handle = tokio::spawn(runner_adaptive::run(AdaptiveRunnerParams {
            reaction_name: "test-grpc".to_string(),
            adaptive: AdaptiveBatchConfig::default(),
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::AutoSkipGap,
        }));

        // First item: every retry is rejected, so the batch is dropped.
        base.enqueue_query_result(query_result("q1", 1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            base.get_status().await,
            ComponentStatus::Running,
            "adaptive reaction must stay Running after a skipped batch under AutoSkipGap"
        );

        // Later item: the server now accepts, proving the reaction kept running.
        base.enqueue_query_result(query_result("q1", 2))
            .await
            .unwrap();
        let total = server
            .recorder
            .wait_for_items(1, Duration::from_secs(5))
            .await;
        assert_eq!(total, 1);
        assert_eq!(base.get_status().await, ComponentStatus::Running);

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn adaptive_runner_fail_stops_on_delivery_failure_under_strict() {
        // Under the default Strict policy, a sustained delivery failure must set
        // the reaction to Error (fail-stop) without advancing the checkpoint.
        let server = test_server::start_with_failures(100).await;
        let base = running_base();
        set_running(&base).await;

        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();
        let config = config_for(
            server.endpoint.clone(),
            BatchingConfig::adaptive(AdaptiveBatchConfig::default()),
        );
        let handle = tokio::spawn(runner_adaptive::run(AdaptiveRunnerParams {
            reaction_name: "test-grpc".to_string(),
            adaptive: AdaptiveBatchConfig::default(),
            base: base.clone_shared(),
            config,
            shutdown_rx: sd_rx,
            checkpoints: crate::checkpoint::CheckpointState::load(&base).await,
            policy: drasi_lib::ReactionRecoveryPolicy::Strict,
        }));

        base.enqueue_query_result(query_result("q1", 1))
            .await
            .unwrap();

        let errored = wait_for_status(&base, ComponentStatus::Error, Duration::from_secs(5)).await;
        assert!(
            errored,
            "adaptive reaction must fail-stop (Error) on sustained delivery failure under Strict"
        );

        let _ = sd_tx.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        server.shutdown().await;
    }
}
