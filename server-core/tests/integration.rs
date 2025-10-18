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
use drasi_server_core::{DrasiServerCore, Properties, Query, Reaction, Source};
use serde_json::json;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_library_initialization() -> Result<()> {
    // Use the new builder API
    let core = DrasiServerCore::builder()
        .with_id("test-server")
        .build()
        .await?;

    // Should not be running yet
    assert!(!core.is_running().await);

    Ok(())
}

#[tokio::test]
async fn test_start_stop_lifecycle() -> Result<()> {
    // Use the new builder API
    let core = DrasiServerCore::builder()
        .with_id("test-lifecycle")
        .build()
        .await?;

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
    // Use the builder API
    let core = DrasiServerCore::builder()
        .with_id("test-mock-server")
        .add_source(
            Source::mock("test-mock")
                .with_properties(
                    Properties::new()
                        .with_int("interval_ms", 1000)
                        .with_string("data_type", "counter"),
                )
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

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
    // Use the builder API
    let core = DrasiServerCore::builder()
        .with_id("test-pipeline")
        .add_source(
            Source::mock("source1")
                .with_properties(
                    Properties::new()
                        .with_int("interval_ms", 500)
                        .with_string("data_type", "sensor"),
                )
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .auto_start(true)
                .build(),
        )
        .add_reaction(
            Reaction::log("reaction1")
                .subscribe_to("query1")
                .with_property("log_level", json!("error"))
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    core.start().await?;

    // Let the pipeline run
    sleep(Duration::from_millis(200)).await;

    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_restart_capability() -> Result<()> {
    // Use the builder API
    let core = DrasiServerCore::builder()
        .with_id("test-restart")
        .build()
        .await?;

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
    // Use the builder API with multiple components
    let mut builder = DrasiServerCore::builder().with_id("test-multiple");

    // Add multiple sources
    for i in 1..=3 {
        builder = builder.add_source(
            Source::mock(&format!("source{}", i))
                .with_properties(
                    Properties::new()
                        .with_int("interval_ms", 1000)
                        .with_string("data_type", "counter"),
                )
                .auto_start(true)
                .build(),
        );
    }

    // Add multiple queries
    for i in 1..=3 {
        builder = builder.add_query(
            Query::cypher(&format!("query{}", i))
                .query("MATCH (n) RETURN n")
                .from_source(&format!("source{}", i))
                .auto_start(true)
                .build(),
        );
    }

    let core = builder.build().await?;
    core.start().await?;

    sleep(Duration::from_millis(100)).await;
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

// ============================================================================
// New Builder API Tests
// ============================================================================

#[tokio::test]
async fn test_builder_api_basic() -> Result<()> {
    // Test the new fluent builder API
    let core = DrasiServerCore::builder()
        .with_id("test-builder-server")
        .build()
        .await?;

    // Should be initialized and ready to start
    assert!(!core.is_running().await);

    core.start().await?;
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_builder_api_with_components() -> Result<()> {
    // Test building a complete pipeline with the new API
    let core = DrasiServerCore::builder()
        .with_id("test-pipeline")
        .add_source(
            Source::mock("source1")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "sensor")
                        .with_int("interval_ms", 500),
                )
                .build(),
        )
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_reaction(
            Reaction::log("reaction1")
                .subscribe_to("query1")
                .with_property("log_level", json!("error"))
                .build(),
        )
        .build()
        .await?;

    core.start().await?;
    sleep(Duration::from_millis(200)).await;
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_config_file_loading() -> Result<()> {
    // Create a test config file
    use std::io::Write;
    let mut temp_file = tempfile::NamedTempFile::new()?;
    writeln!(
        temp_file,
        r#"
server:
  id: test-from-file
sources: []
queries: []
reactions: []
"#
    )?;

    // Test loading from config file
    let core = DrasiServerCore::from_config_file(temp_file.path()).await?;

    assert!(!core.is_running().await);
    core.start().await?;
    assert!(core.is_running().await);

    core.stop().await?;
    Ok(())
}
