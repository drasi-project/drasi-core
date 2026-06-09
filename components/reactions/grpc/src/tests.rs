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
        // v3 schema: no string `type`, no `data`; carries item_type enum
        // (encoded as i32 on the wire), explicit row_signature, and an
        // optional templated payload (omitted here).
        use crate::proto::drasi_v1::QueryResultItemType;
        ProtoQueryResultItem {
            before: None,
            after: None,
            item_type: QueryResultItemType::Add as i32,
            row_signature: 0,
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

    async fn set_running(base: &ReactionBase) {
        base.set_status(ComponentStatus::Running, None).await;
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
}
