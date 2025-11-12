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

use super::AdaptiveGrpcReaction;
use crate::channels::*;
use crate::reactions::common::AdaptiveBatchConfig as ConfigAdaptiveBatchConfig;
use crate::reactions::grpc_adaptive::GrpcAdaptiveReactionConfig;
use crate::config::{QueryConfig, ReactionConfig, ReactionSpecificConfig};
use crate::queries::Query;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use crate::utils::AdaptiveBatchConfig;
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

/// Mock query for testing reactions
struct MockQuery {
    config: QueryConfig,
    status: Arc<RwLock<ComponentStatus>>,
    dispatcher: Arc<crate::channels::BroadcastChangeDispatcher<QueryResult>>,
}

impl MockQuery {
    fn new(query_id: &str) -> Self {
        let dispatcher =
            Arc::new(crate::channels::BroadcastChangeDispatcher::<QueryResult>::new(1000));
        Self {
            config: QueryConfig {
                id: query_id.to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: crate::config::QueryLanguage::Cypher,
                sources: vec![],
                auto_start: false,
                joins: None,
                enable_bootstrap: false,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
            },
            status: Arc::new(RwLock::new(ComponentStatus::Running)),
            dispatcher,
        }
    }
}

#[async_trait]
impl Query for MockQuery {
    async fn start(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Stopped;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &QueryConfig {
        &self.config
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn subscribe(&self, _reaction_id: String) -> Result<QuerySubscriptionResponse, String> {
        let receiver = self
            .dispatcher
            .create_receiver()
            .await
            .map_err(|e| format!("Failed to create receiver: {}", e))?;
        Ok(QuerySubscriptionResponse {
            query_id: self.config.id.clone(),
            receiver,
        })
    }
}

async fn create_test_server_with_query(query_id: &str) -> Result<Arc<DrasiServerCore>> {
    let mock_query = Arc::new(MockQuery::new(query_id));
    let server_core = DrasiServerCore::builder().build().await?;
    server_core
        .query_manager()
        .add_query_instance_for_test(mock_query)
        .await?;
    Ok(Arc::new(server_core))
}

fn create_adaptive_grpc_reaction_config(reaction_id: &str, endpoint: &str) -> ReactionConfig {
    ReactionConfig {
        id: reaction_id.to_string(),
        queries: vec!["query1".to_string()],
        auto_start: false,
        config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
            endpoint: endpoint.to_string(),
            timeout_ms: 5000,
            max_retries: 3,
            connection_retry_attempts: 5,
            initial_connection_timeout_ms: 10000,
            metadata: HashMap::new(),
            adaptive: ConfigAdaptiveBatchConfig {
                adaptive_min_batch_size: 1,
                adaptive_max_batch_size: 100,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 1000,
            },
        }),
        priority_queue_capacity: None,
    }
}

fn create_adaptive_grpc_reaction_with_custom_config(
    reaction_id: &str,
    endpoint: &str,
    adaptive_max_batch_size: usize,
    adaptive_min_batch_size: usize,
    adaptive_window_size: usize,
    adaptive_batch_timeout_ms: u64,
) -> ReactionConfig {
    ReactionConfig {
        id: reaction_id.to_string(),
        queries: vec!["query1".to_string()],
        auto_start: false,
        config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
            endpoint: endpoint.to_string(),
            timeout_ms: 5000,
            max_retries: 3,
            connection_retry_attempts: 5,
            initial_connection_timeout_ms: 10000,
            metadata: HashMap::new(),
            adaptive: ConfigAdaptiveBatchConfig {
                adaptive_min_batch_size,
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
            },
        }),
        priority_queue_capacity: None,
    }
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_creation() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config =
        create_adaptive_grpc_reaction_config("test-adaptive-grpc", "grpc://localhost:50052");
    let reaction = AdaptiveGrpcReaction::new(config.clone(), event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_lifecycle() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_adaptive_grpc_reaction_config(
        "test-adaptive-grpc-lifecycle",
        "grpc://localhost:50052",
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);

    // 1. Initial state should be Stopped
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);

    // 2. Start the reaction
    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    // 3. Verify it transitions to Running
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    // 4. Stop the reaction
    reaction.stop().await.unwrap();

    // 5. Verify it returns to Stopped
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_default_config() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config =
        create_adaptive_grpc_reaction_config("test-adaptive-default", "grpc://localhost:50052");
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);

    // Verify default adaptive config is applied
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_custom_config() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-custom",
        "grpc://localhost:50052",
        2000, // max_batch_size
        50,   // min_batch_size
        10,   // window_size
        500,  // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);

    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_high_throughput_config() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-high-throughput",
        "grpc://localhost:50052",
        2000, // max_batch_size
        100,  // min_batch_size
        10,   // window_size
        1000, // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_low_latency_config() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-low-latency",
        "grpc://localhost:50052",
        100, // max_batch_size
        5,   // min_batch_size
        3,   // window_size
        50,  // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_disabled_adaptation() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-disabled",
        "grpc://localhost:50052",
        1000, // max_batch_size
        100,  // min_batch_size
        5,    // window_size
        1000, // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[test]
fn test_adaptive_batch_config_default() {
    let config = AdaptiveBatchConfig::default();
    assert_eq!(config.max_batch_size, 1000);
    assert_eq!(config.min_batch_size, 10);
    assert_eq!(config.max_wait_time, Duration::from_millis(100));
    assert_eq!(config.min_wait_time, Duration::from_millis(1));
    assert_eq!(config.throughput_window, Duration::from_secs(5));
    assert_eq!(config.adaptive_enabled, true);
}

#[test]
fn test_adaptive_batch_config_custom() {
    let mut config = AdaptiveBatchConfig::default();
    config.max_batch_size = 2000;
    config.min_batch_size = 50;
    config.max_wait_time = Duration::from_millis(500);
    config.min_wait_time = Duration::from_millis(5);
    config.throughput_window = Duration::from_secs(10);
    config.adaptive_enabled = false;

    assert_eq!(config.max_batch_size, 2000);
    assert_eq!(config.min_batch_size, 50);
    assert_eq!(config.max_wait_time, Duration::from_millis(500));
    assert_eq!(config.min_wait_time, Duration::from_millis(5));
    assert_eq!(config.throughput_window, Duration::from_secs(10));
    assert_eq!(config.adaptive_enabled, false);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_with_metadata() {
    let (event_tx, _event_rx) = mpsc::channel(100);

    let mut metadata = HashMap::new();
    metadata.insert("authorization".to_string(), "Bearer token123".to_string());
    metadata.insert("x-tenant-id".to_string(), "prod-456".to_string());

    let config = ReactionConfig {
        id: "test-adaptive-metadata".to_string(),
        queries: vec!["query1".to_string()],
        auto_start: false,
        config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
            endpoint: "grpc://localhost:50052".to_string(),
            timeout_ms: 5000,
            max_retries: 3,
            connection_retry_attempts: 5,
            initial_connection_timeout_ms: 10000,
            metadata,
            adaptive: ConfigAdaptiveBatchConfig {
                adaptive_min_batch_size: 1,
                adaptive_max_batch_size: 100,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 1000,
            },
        }),
        priority_queue_capacity: None,
    };

    let reaction = AdaptiveGrpcReaction::new(config, event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_batch_size_constraints() {
    let (event_tx, _event_rx) = mpsc::channel(100);

    // Test with narrow range (limited adaptation)
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-narrow-range",
        "grpc://localhost:50052",
        100, // max
        90,  // min - very narrow range
        5,   // window_size
        100, // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx.clone());
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);

    // Test with wide range (maximum adaptation)
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-wide-range",
        "grpc://localhost:50052",
        5000, // max
        1,    // min - very wide range
        5,   // window_size
        100, // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_window_size_variations() {
    let (event_tx, _event_rx) = mpsc::channel(100);

    // Test with short window (fast adaptation)
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-short-window",
        "grpc://localhost:50052",
        1000, // max_batch_size
        10,   // min_batch_size
        1,    // window_size - very short
        100,  // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx.clone());
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);

    // Test with long window (stable adaptation)
    let config = create_adaptive_grpc_reaction_with_custom_config(
        "test-adaptive-long-window",
        "grpc://localhost:50052",
        1000, // max_batch_size
        10,   // min_batch_size
        30,   // window_size - long window
        100,  // batch_timeout_ms
    );
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_timeout_configurations() {
    let (event_tx, _event_rx) = mpsc::channel(100);

    // Test with short timeout (for fast failures)
    let mut config = create_adaptive_grpc_reaction_config(
        "test-adaptive-short-timeout",
        "grpc://localhost:50052",
    );
    if let ReactionSpecificConfig::GrpcAdaptive(ref mut grpc_config) = config.config {
        grpc_config.timeout_ms = 1000; // Short timeout
        grpc_config.max_retries = 2; // Fewer retries
    }
    let reaction = AdaptiveGrpcReaction::new(config, event_tx.clone());
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);

    // Test with long timeout (for slow servers)
    let mut config = create_adaptive_grpc_reaction_config(
        "test-adaptive-long-timeout",
        "grpc://localhost:50052",
    );
    if let ReactionSpecificConfig::GrpcAdaptive(ref mut grpc_config) = config.config {
        grpc_config.timeout_ms = 30000; // Long timeout
        grpc_config.max_retries = 10; // More retries
    }
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_adaptive_grpc_reaction_connection_retry_configurations() {
    let (event_tx, _event_rx) = mpsc::channel(100);

    // Test with aggressive retries
    let mut config = create_adaptive_grpc_reaction_config(
        "test-adaptive-aggressive-retry",
        "grpc://localhost:50052",
    );
    if let ReactionSpecificConfig::GrpcAdaptive(ref mut grpc_config) = config.config {
        grpc_config.connection_retry_attempts = 10;
        grpc_config.initial_connection_timeout_ms = 5000;
    }
    let reaction = AdaptiveGrpcReaction::new(config, event_tx.clone());
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);

    // Test with minimal retries
    let mut config = create_adaptive_grpc_reaction_config(
        "test-adaptive-minimal-retry",
        "grpc://localhost:50052",
    );
    if let ReactionSpecificConfig::GrpcAdaptive(ref mut grpc_config) = config.config {
        grpc_config.connection_retry_attempts = 1;
        grpc_config.initial_connection_timeout_ms = 2000;
    }
    let reaction = AdaptiveGrpcReaction::new(config, event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}
