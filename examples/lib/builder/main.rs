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

//! # DrasiLib Builder Example
//!
//! This example demonstrates programmatic use of drasi-lib with an HTTP source
//! and a single query. It shows:
//!
//! - Creating an HTTP source to receive price updates
//! - Using ScriptFile bootstrap provider for initial data
//! - Defining a single Cypher query for continuous monitoring
//! - Attaching a Log reaction to print results to console
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
use drasi_lib::{DrasiLib, Query};
use drasi_source_http::HttpSource;
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging - set RUST_LOG=debug for more detail
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")
    ).init();

    println!("========================================");
    println!("  DrasiLib Builder Example");
    println!("========================================\n");

    // =========================================================================
    // Step 1: Create Bootstrap Provider
    // =========================================================================
    // The ScriptFile bootstrap provider loads initial stock data from a JSONL
    // file. This data is used to populate the query when it first starts.

    let bootstrap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bootstrap_data.jsonl");

    println!("Loading bootstrap data from: {}", bootstrap_path.display());

    let bootstrap_provider = ScriptFileBootstrapProvider::builder()
        .with_file(bootstrap_path.to_string_lossy().to_string())
        .build();

    // =========================================================================
    // Step 2: Create HTTP Source
    // =========================================================================
    // The HTTP source exposes endpoints to receive stock price updates.
    // Events are sent to: POST /sources/stock-prices/events

    // Adaptive batching optimizes throughput for high-volume scenarios
    // The bootstrap provider is attached directly in the builder
    let http_source = HttpSource::builder("stock-prices")
        .with_host("0.0.0.0")
        .with_port(9000)
        .with_adaptive_enabled(true)
        .with_adaptive_max_batch_size(100)
        .with_adaptive_min_batch_size(1)
        .with_adaptive_max_wait_ms(50)
        .with_adaptive_min_wait_ms(10)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    // =========================================================================
    // Step 3: Define Query
    // =========================================================================
    // The query continuously monitors the stock_prices nodes and emits
    // results whenever the underlying data changes.

    let all_prices_query = Query::cypher("all-prices")
        .query(r#"
            MATCH (sp:stock_prices)
            RETURN sp.symbol AS symbol,
                   sp.price AS price,
                   sp.previous_close AS previous_close,
                   sp.volume AS volume
        "#)
        .from_source("stock-prices")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    // =========================================================================
    // Step 4: Create Log Reaction
    // =========================================================================
    // The LogReaction prints query results directly to the console.
    // Templates use Handlebars syntax to format each event type.

    let log_reaction = LogReaction::builder("console-logger")
        .from_query("all-prices")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("[+] {{after.symbol}}: ${{after.price}} (prev: ${{after.previous_close}}, vol: {{after.volume}})")),
            updated: Some(TemplateSpec::new("[~] {{after.symbol}}: ${{before.price}} -> ${{after.price}} (vol: {{after.volume}})")),
            deleted: Some(TemplateSpec::new("[-] {{before.symbol}} removed")),
        })
        .build()?;

    // =========================================================================
    // Step 5: Build DrasiLib
    // =========================================================================
    // Assemble all components into a DrasiLib instance. The builder pattern
    // takes ownership of sources, queries, and reactions.

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("http-source-example")
            .with_source(http_source)
            .with_query(all_prices_query)
            .with_reaction(log_reaction)
            .build()
            .await?
    );

    // =========================================================================
    // Step 6: Start Processing
    // =========================================================================
    // Starting the core initializes all components:
    // 1. Sources start listening for events
    // 2. Queries subscribe to sources and run bootstrap
    // 3. Reactions auto-start by default (like queries)

    core.start().await?;

    // =========================================================================
    // Step 7: Start Results API Server
    // =========================================================================
    // A simple HTTP server on port 8080 to view query results via REST API.
    // This is for demonstration purposes only so the current query results
    // can be viewed.

    let api_core = core.clone();
    let results_api = Router::new()
        .route("/queries/:id/results", get(get_query_results))
        .with_state(api_core);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("\n----------------------------------------");
    println!(" Builder Example Started!");
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