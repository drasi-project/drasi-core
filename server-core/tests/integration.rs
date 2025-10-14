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

//! Integration tests for DrasiServerCore library
//!
//! These tests verify the library can be used as intended by external consumers.

use anyhow::Result;
use drasi_server_core::{
    config::{DrasiServerCoreSettings, QueryConfig, QueryLanguage, ReactionConfig, SourceConfig},
    DrasiServerCore, RuntimeConfig,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Helper to create a minimal test configuration
fn create_minimal_config() -> RuntimeConfig {
    RuntimeConfig {
        server: DrasiServerCoreSettings {
            id: "test-server".to_string(),
            log_level: "error".to_string(),
            max_connections: 100,
            shutdown_timeout_seconds: 30,
            disable_persistence: false,
        },
        sources: vec![],
        queries: vec![],
        reactions: vec![],
    }
}

#[tokio::test]
async fn test_library_initialization() -> Result<()> {
    let config = Arc::new(create_minimal_config());
    let mut core = DrasiServerCore::new(config);

    // Should be able to initialize
    core.initialize().await?;

    // Should not be running yet
    assert!(!core.is_running().await);

    Ok(())
}

#[tokio::test]
async fn test_start_stop_lifecycle() -> Result<()> {
    let config = Arc::new(create_minimal_config());
    let mut core = DrasiServerCore::new(config);

    core.initialize().await?;

    // Start the server
    core.start().await?;
    assert!(core.is_running().await);

    // Stop the server
    core.stop().await?;
    assert!(!core.is_running().await);

    Ok(())
}

#[tokio::test]
async fn test_with_mock_source() -> Result<()> {
    let mut config = create_minimal_config();

    // Add a mock source
    config.sources.push(SourceConfig {
        id: "test-mock".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: HashMap::from([
            ("interval_ms".to_string(), serde_json::json!(1000)),
            ("data_type".to_string(), serde_json::json!("counter")),
        ]),
        bootstrap_provider: None,
    });

    let mut core = DrasiServerCore::new(Arc::new(config));
    core.initialize().await?;
    core.start().await?;

    // Let it run briefly
    sleep(Duration::from_millis(100)).await;

    // Should still be running
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_with_query_and_reaction() -> Result<()> {
    let mut config = create_minimal_config();

    // Add a complete pipeline
    config.sources.push(SourceConfig {
        id: "source1".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: HashMap::from([
            ("interval_ms".to_string(), serde_json::json!(500)),
            ("data_type".to_string(), serde_json::json!("sensor")),
        ]),
        bootstrap_provider: None,
    });

    config.queries.push(QueryConfig {
        id: "query1".to_string(),
        query: "MATCH (n) RETURN n".to_string(),
        query_language: QueryLanguage::Cypher,
        sources: vec!["source1".to_string()],
        auto_start: true,
        properties: HashMap::new(),
        joins: None,
    });

    config.reactions.push(ReactionConfig {
        id: "reaction1".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["query1".to_string()],
        auto_start: true,
        properties: HashMap::from([("log_level".to_string(), serde_json::json!("error"))]),
    });

    let mut core = DrasiServerCore::new(Arc::new(config));
    core.initialize().await?;
    core.start().await?;

    // Let the pipeline run
    sleep(Duration::from_millis(200)).await;

    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_restart_capability() -> Result<()> {
    let config = Arc::new(create_minimal_config());
    let mut core = DrasiServerCore::new(config);

    core.initialize().await?;

    // Start
    core.start().await?;
    assert!(core.is_running().await);

    // Stop
    core.stop().await?;
    assert!(!core.is_running().await);

    // Restart
    core.start().await?;
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_multiple_sources_and_queries() -> Result<()> {
    let mut config = create_minimal_config();

    // Add multiple sources
    for i in 1..=3 {
        config.sources.push(SourceConfig {
            id: format!("source{}", i),
            source_type: "mock".to_string(),
            auto_start: true,
            properties: HashMap::from([
                ("interval_ms".to_string(), serde_json::json!(1000)),
                ("data_type".to_string(), serde_json::json!("counter")),
            ]),
            bootstrap_provider: None,
        });
    }

    // Add multiple queries
    for i in 1..=3 {
        config.queries.push(QueryConfig {
            id: format!("query{}", i),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            sources: vec![format!("source{}", i)],
            auto_start: true,
            properties: HashMap::new(),
            joins: None,
        });
    }

    let mut core = DrasiServerCore::new(Arc::new(config));
    core.initialize().await?;
    core.start().await?;

    sleep(Duration::from_millis(100)).await;
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}
