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

//! # DrasiLib Constructor Example
//!
//! This example demonstrates programmatic use of drasi-lib using config structs
//! and the DrasiLib API instead of builder patterns. It shows:
//!
//! - Creating components using config structs and constructors
//! - Dynamically adding sources, queries, and reactions to DrasiLib
//! - Using the DrasiLib API (`add_source`, `create_query`, `add_reaction`)
//!
//! This approach is useful when you want to:
//! - Add sources and reactions dynamically at runtime
//! - Manage component lifecycle programmatically
//! - Build configuration from external sources (databases, APIs, etc.)
//!
//! ## Running
//!
//! ```bash
//! cargo run
//! ```
//!
//! ## Testing
//!
//! Use the change.http file with VS Code REST Client or curl:
//!
//! ```bash
//! curl -X POST http://localhost:9000/sources/stock-prices/events \
//!   -H "Content-Type: application/json" \
//!   -d '{"operation":"update","element":{"type":"node","id":"price_AAPL","labels":["stock_prices"],"properties":{"symbol":"AAPL","price":180.00,"previous_close":173.00,"volume":7500000}}}'
//! ```

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

// DrasiLib core types - using config structs instead of builders
use drasi_lib::{
    DrasiLib, QueryConfig, QueryLanguage,
    // SourceSubscriptionConfig is in config module
    config::SourceSubscriptionConfig,
    // Import SourceTrait to access set_bootstrap_provider method
    SourceTrait,
};

// Component types and configs - using constructors instead of builders
use drasi_source_http::{HttpSource, HttpSourceConfig};
use drasi_bootstrap_scriptfile::{ScriptFileBootstrapProvider, ScriptFileBootstrapConfig};
use drasi_reaction_log::{LogReaction, LogReactionConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging - set RUST_LOG=debug for more detail
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")
    ).init();

    println!("========================================");
    println!("  DrasiLib Constructor Example");
    println!("========================================\n");

    // =========================================================================
    // Step 1: Create DrasiLib Instance (Empty)
    // =========================================================================
    // Create a minimal DrasiLib instance. Components will be added dynamically.
    // This demonstrates the API-based approach where you build up the system
    // programmatically rather than declaring everything upfront with builders.

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("http-source-api-example")
            .build()
            .await?
    );

    println!("Created DrasiLib instance: http-source-api-example\n");

    // =========================================================================
    // Step 2: Create Bootstrap Provider using Config Struct
    // =========================================================================
    // The ScriptFile bootstrap provider loads initial stock data from a JSONL
    // file. Here we use the config struct directly instead of a builder.

    let bootstrap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bootstrap_data.jsonl");

    println!("Loading bootstrap data from: {}", bootstrap_path.display());

    // Create config struct directly
    let bootstrap_config = ScriptFileBootstrapConfig {
        file_paths: vec![bootstrap_path.to_string_lossy().to_string()],
    };

    // Create provider using the constructor with config
    let bootstrap_provider = ScriptFileBootstrapProvider::new(bootstrap_config);

    // =========================================================================
    // Step 3: Create HTTP Source using Config Struct
    // =========================================================================
    // Create the HTTP source configuration struct directly and use the
    // HttpSource::new() constructor instead of HttpSource::builder().

    // Create HTTP source config struct
    let http_config = HttpSourceConfig {
        host: "0.0.0.0".to_string(),
        port: 9000,
        endpoint: None,
        timeout_ms: 10000,
        // Adaptive batching settings
        adaptive_enabled: Some(true),
        adaptive_max_batch_size: Some(100),
        adaptive_min_batch_size: Some(1),
        adaptive_max_wait_ms: Some(50),
        adaptive_min_wait_ms: Some(10),
        adaptive_window_secs: None,
    };

    // Create source using constructor
    let http_source = HttpSource::new("stock-prices", http_config)?;

    // Set the bootstrap provider on the source
    // Note: This must be done before adding the source to DrasiLib
    http_source.set_bootstrap_provider(Box::new(bootstrap_provider)).await;

    // =========================================================================
    // Step 4: Add Source to DrasiLib Dynamically
    // =========================================================================
    // Use the DrasiLib API to add the source. This demonstrates dynamic
    // component management rather than builder-based configuration.

    core.add_source(http_source).await?;
    println!("Added HTTP source: stock-prices");

    // =========================================================================
    // Step 5: Create Query using Config Struct
    // =========================================================================
    // Create the QueryConfig struct directly instead of using Query::cypher().

    let query_config = QueryConfig {
        id: "all-prices".to_string(),
        query: r#"
            MATCH (sp:stock_prices)
            RETURN sp.symbol AS symbol,
                   sp.price AS price,
                   sp.previous_close AS previous_close,
                   sp.volume AS volume
        "#.to_string(),
        query_language: QueryLanguage::Cypher,
        middleware: vec![],
        sources: vec![
            SourceSubscriptionConfig {
                source_id: "stock-prices".to_string(),
                pipeline: vec![],
            }
        ],
        auto_start: true,
        joins: None,
        enable_bootstrap: true,
        bootstrap_buffer_size: 10000,
        priority_queue_capacity: None,
        dispatch_buffer_capacity: None,
        dispatch_mode: None,
        storage_backend: None,
    };

    // =========================================================================
    // Step 6: Add Query to DrasiLib Dynamically
    // =========================================================================
    // Use the DrasiLib API to create the query.

    core.add_query(query_config).await?;
    println!("Added query: all-prices");

    // =========================================================================
    // Step 7: Create Log Reaction using Config Struct
    // =========================================================================
    // Create the LogReactionConfig struct directly and use the
    // LogReaction::new() constructor instead of LogReaction::builder().

    // Create reaction config struct
    let log_config = LogReactionConfig {
        added_template: Some("[+] {{after.symbol}}: ${{after.price}} (prev: ${{after.previous_close}}, vol: {{after.volume}})".to_string()),
        updated_template: Some("[~] {{after.symbol}}: ${{before.price}} -> ${{after.price}} (vol: {{after.volume}})".to_string()),
        deleted_template: Some("[-] {{before.symbol}} removed".to_string()),
    };

    // Create reaction using constructor with:
    // - id: unique identifier
    // - queries: list of query IDs to subscribe to
    // - config: reaction-specific configuration
    let log_reaction = LogReaction::new(
        "console-logger",
        vec!["all-prices".to_string()],
        log_config,
    );

    // =========================================================================
    // Step 8: Add Reaction to DrasiLib Dynamically
    // =========================================================================
    // Use the DrasiLib API to add the reaction.

    core.add_reaction(log_reaction).await?;
    println!("Added reaction: console-logger\n");

    // =========================================================================
    // Step 9: Start Processing
    // =========================================================================
    // Starting the core initializes all components:
    // 1. Sources start listening for events
    // 2. Queries subscribe to sources and run bootstrap
    // 3. Reactions auto-start by default (like queries)

    core.start().await?;

    // =========================================================================
    // Step 10: Start Results API Server
    // =========================================================================
    // A simple HTTP server on port 8080 to view query results via REST API.

    let api_core = core.clone();
    let results_api = Router::new()
        .route("/queries/:id/results", get(get_query_results))
        .with_state(api_core);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("----------------------------------------");
    println!(" Constructor Example Started!");
    println!("----------------------------------------");
    println!(" HTTP Source: http://localhost:9000");
    println!("   POST /sources/stock-prices/events");
    println!("   GET  /health");
    println!("----------------------------------------");
    println!(" Query: all-prices");
    println!("----------------------------------------");
    println!(" Results API: http://localhost:8080");
    println!("   GET /queries/all-prices/results");
    println!("----------------------------------------");
    println!(" Press Ctrl+C to stop");
    println!("----------------------------------------\n");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;

    println!("\n>>> Shutting down gracefully...");
    api_handle.abort();
    core.stop().await?;
    println!(">>> Shutdown complete.");

    Ok(())
}

/// Handler for GET /queries/:id/results
async fn get_query_results(
    State(core): State<Arc<DrasiLib>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, (axum::http::StatusCode, String)> {
    core.get_query_results(&id)
        .await
        .map(Json)
        .map_err(|e| (axum::http::StatusCode::NOT_FOUND, e.to_string()))
}
