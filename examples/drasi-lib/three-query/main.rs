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

//! # DrasiLib Stock Monitor Example
//!
//! This example demonstrates programmatic use of drasi-lib to build a
//! real-time stock price monitoring system. It shows:
//!
//! - Creating an HTTP source to receive price updates
//! - Using ScriptFile bootstrap provider for initial data
//! - Defining multiple Cypher queries for continuous monitoring
//! - Attaching a Log reaction to print results to console
//!
//! ## Running
//!
//! ```bash
//! cargo run --example drasi-lib-stocks
//! ```
//!
//! ## Testing
//!
//! Use the api.http file with VS Code REST Client or curl:
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
use drasi_reaction_log::LogReaction;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging - set RUST_LOG=debug for more detail
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")
    ).init();

    println!("╔════════════════════════════════════════════╗");
    println!("║     DrasiLib Stock Monitor Example         ║");
    println!("╚════════════════════════════════════════════╝\n");

    // =========================================================================
    // Step 1: Create Bootstrap Provider
    // =========================================================================
    // The ScriptFile bootstrap provider loads initial stock data from a JSONL
    // file. This data is used to populate queries when they first start.

    let bootstrap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bootstrap_data.jsonl");

    println!("Loading bootstrap data from: {}", bootstrap_path.display());

    let bootstrap_provider = ScriptFileBootstrapProvider::new(vec![
        bootstrap_path.to_string_lossy().to_string()
    ]);

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
    // Step 3: Define Queries
    // =========================================================================
    // Each query continuously monitors the stock_prices nodes and emits
    // results whenever the underlying data changes.

    // Query 1: All Prices - Returns all stock prices with their details
    let all_prices = Query::cypher("all-prices")
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

    // Query 2: Gainers - Stocks where current price exceeds previous close
    let gainers = Query::cypher("gainers")
        .query(r#"
            MATCH (sp:stock_prices)
            WHERE sp.price > sp.previous_close
            RETURN sp.symbol AS symbol,
                   sp.price AS price,
                   sp.previous_close AS previous_close,
                   ((sp.price - sp.previous_close) / sp.previous_close * 100) AS gain_percent
        "#)
        .from_source("stock-prices")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    // Query 3: High Volume - Stocks with trading volume over 1 million
    let high_volume = Query::cypher("high-volume")
        .query(r#"
            MATCH (sp:stock_prices)
            WHERE sp.volume > 1000000
            RETURN sp.symbol AS symbol,
                   sp.price AS price,
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
    // Since this reaction subscribes to multiple queries, we include query_name
    // to identify which query produced each result.

    let log_reaction = LogReaction::builder("console-logger")
        .from_query("all-prices")
        .from_query("gainers")
        .from_query("high-volume")
        .with_added_template("[{{query_name}}] + {{after.symbol}}: ${{after.price}}")
        .with_updated_template("[{{query_name}}] ~ {{after.symbol}}: ${{before.price}} -> ${{after.price}}")
        .with_deleted_template("[{{query_name}}] - {{before.symbol}} removed")
        .build();

    // =========================================================================
    // Step 5: Build DrasiLib
    // =========================================================================
    // Assemble all components into a DrasiLib instance. The builder pattern
    // takes ownership of sources and reactions.

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("stock-monitor")
            .with_source(http_source)
            .with_query(all_prices)
            .with_query(gainers)
            .with_query(high_volume)
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
    // A simple HTTP server on port 8080 to query results via REST API.

    let api_core = core.clone();
    let results_api = Router::new()
        .route("/queries/:id/results", get(get_query_results))
        .with_state(api_core);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("\n┌────────────────────────────────────────────┐");
    println!("│ Stock Monitor Started Successfully!        │");
    println!("├────────────────────────────────────────────┤");
    println!("│ HTTP Source: http://localhost:9000         │");
    println!("│   POST /sources/stock-prices/events        │");
    println!("│   POST /sources/stock-prices/events/batch  │");
    println!("│   GET  /health                             │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Queries:                                   │");
    println!("│   • all-prices  - All stock prices         │");
    println!("│   • gainers     - Stocks with gains        │");
    println!("│   • high-volume - Volume > 1M              │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Results API: http://localhost:8080         │");
    println!("│   GET /queries/all-prices/results          │");
    println!("│   GET /queries/gainers/results             │");
    println!("│   GET /queries/high-volume/results         │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Press Ctrl+C to stop                       │");
    println!("└────────────────────────────────────────────┘\n");

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