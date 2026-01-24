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

//! # DrasiLib with RocksDB Index Example
//!
//! This example demonstrates how to use an external index plugin (RocksDB) with
//! drasi-lib. By default, drasi-lib uses in-memory indexes which are volatile.
//! For production use cases requiring persistence, you can inject an index
//! provider like RocksDB or Garnet.
//!
//! ## Key Concepts
//!
//! - **Default behavior**: Without an index provider, DrasiLib uses in-memory indexes
//! - **With RocksDB plugin**: Data persists across restarts
//! - **Plugin injection**: Use `.with_index_provider(Arc::new(provider))` in the builder
//!
//! ## Running
//!
//! ```bash
//! cargo run
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

// Import the RocksDB index plugin
use drasi_index_rocksdb::RocksDbIndexProvider;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging - set RUST_LOG=debug for more detail
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")
    ).init();

    println!("========================================");
    println!("  DrasiLib with RocksDB Index Example");
    println!("========================================\n");

    // =========================================================================
    // Step 1: Create RocksDB Index Provider
    // =========================================================================
    // The RocksDbIndexProvider implements the IndexBackendPlugin trait.
    // This provides persistent storage for query indexes, allowing data to
    // survive restarts.
    //
    // Arguments:
    //   - path: Directory where RocksDB will store data
    //   - enable_archive: Enable archive index for past() function support
    //   - direct_io: Use direct I/O (bypasses OS cache)

    let data_dir = tempfile::tempdir()?;
    let rocksdb_path = data_dir.path().to_path_buf();

    println!("Using RocksDB data directory: {}", rocksdb_path.display());

    let rocksdb_provider = RocksDbIndexProvider::new(
        rocksdb_path,
        true,   // enable_archive: support for past() function
        false,  // direct_io: let OS manage caching
    );

    // =========================================================================
    // Step 2: Create Bootstrap Provider
    // =========================================================================

    let bootstrap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Cannot navigate to parent directory for bootstrap file")
        .join("builder")
        .join("bootstrap_data.jsonl");

    println!("Loading bootstrap data from: {}", bootstrap_path.display());

    let bootstrap_provider = ScriptFileBootstrapProvider::builder()
        .with_file(bootstrap_path.to_string_lossy().to_string())
        .build();

    // =========================================================================
    // Step 3: Create HTTP Source
    // =========================================================================

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
    // Step 4: Define Query
    // =========================================================================

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
    // Step 5: Create Log Reaction
    // =========================================================================

    let log_reaction = LogReaction::builder("console-logger")
        .from_query("all-prices")
        .build()?;

    // =========================================================================
    // Step 6: Build DrasiLib with RocksDB Index Provider
    // =========================================================================
    // The key difference from the basic example: we inject the RocksDB provider
    // using `.with_index_provider()`. This tells DrasiLib to use RocksDB for
    // all query index storage instead of in-memory indexes.

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("rocksdb-example")
            // Inject the RocksDB index provider
            .with_index_provider(Arc::new(rocksdb_provider))
            .with_source(http_source)
            .with_query(all_prices_query)
            .with_reaction(log_reaction)
            .build()
            .await?
    );

    // =========================================================================
    // Step 7: Start Processing
    // =========================================================================

    core.start().await?;

    // =========================================================================
    // Step 8: Start Results API Server
    // =========================================================================

    let api_core = core.clone();
    let results_api = Router::new()
        .route("/queries/:id/results", get(get_query_results))
        .with_state(api_core);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("\n----------------------------------------");
    println!(" RocksDB Index Example Started!");
    println!("----------------------------------------");
    println!(" Index Backend: RocksDB (persistent)");
    println!(" Data Dir: {}", data_dir.path().display());
    println!("----------------------------------------");
    println!(" HTTP Source: http://localhost:9000");        // DevSkim: ignore DS162092
    println!("   POST /sources/stock-prices/events");
    println!("   GET  /health");
    println!("----------------------------------------");
    println!(" Query: all-prices");
    println!("----------------------------------------");
    println!(" Results API: http://localhost:8080");        // DevSkim: ignore DS162092
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
