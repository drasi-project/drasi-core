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

use super::LogReaction;
use crate::channels::*;
use crate::config::common::LogLevel;
use crate::config::{QueryConfig, ReactionConfig, ReactionSpecificConfig};
use crate::queries::Query;
use crate::reactions::log::LogReactionConfig;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
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
                middleware: vec![],
                source_subscriptions: vec![],
                auto_start: false,
                joins: None,
                enable_bootstrap: false,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
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

fn create_log_reaction_config(reaction_id: &str, log_level: LogLevel) -> ReactionConfig {
    ReactionConfig {
        id: reaction_id.to_string(),
        queries: vec!["query1".to_string()],
        auto_start: false,
        config: ReactionSpecificConfig::Log(LogReactionConfig { log_level }),
        priority_queue_capacity: None,
    }
}

#[tokio::test]
async fn test_log_reaction_creation() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log", LogLevel::Info);
    let reaction = LogReaction::new(config.clone(), event_tx);
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_processes_results() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log", LogLevel::Info);
    let reaction = LogReaction::new(config, event_tx);

    // Create test server with mock query
    let server_core = create_test_server_with_query("query1").await.unwrap();

    // Start the reaction (it will subscribe to the mock query)
    reaction.start(server_core).await.unwrap();

    // Verify reaction is running
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    // Stop the reaction
    reaction.stop().await.unwrap();

    // Verify reaction is stopped
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_trace_level() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log-trace", LogLevel::Trace);
    let reaction = LogReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_debug_level() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log-debug", LogLevel::Debug);
    let reaction = LogReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_info_level() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log-info", LogLevel::Info);
    let reaction = LogReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_warn_level() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log-warn", LogLevel::Warn);
    let reaction = LogReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_error_level() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log-error", LogLevel::Error);
    let reaction = LogReaction::new(config, event_tx);

    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_default_log_level() {
    let (event_tx, _event_rx) = mpsc::channel(100);

    // Create config without specifying log level (should default to Info)
    let config = ReactionConfig {
        id: "test-log-default".to_string(),
        queries: vec!["query1".to_string()],
        auto_start: false,
        config: ReactionSpecificConfig::Log(LogReactionConfig {
            log_level: LogLevel::Info,
        }),
        priority_queue_capacity: None,
    };

    let reaction = LogReaction::new(config, event_tx);
    let server_core = create_test_server_with_query("query1").await.unwrap();
    reaction.start(server_core).await.unwrap();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Running);

    reaction.stop().await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn test_log_reaction_lifecycle() {
    let (event_tx, _event_rx) = mpsc::channel(100);
    let config = create_log_reaction_config("test-log-lifecycle", LogLevel::Info);
    let reaction = LogReaction::new(config, event_tx);

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
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(reaction.status().await, ComponentStatus::Stopped);
}
