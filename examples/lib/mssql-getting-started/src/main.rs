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

//! # DrasiLib MS SQL Getting Started Example
//!
//! This example demonstrates programmatic use of drasi-lib with MS SQL Server
//! to build a real-time order monitoring system. It shows:
//!
//! - Creating an MS SQL source with CDC enabled
//! - Using MS SQL bootstrapper for initial data load
//! - Defining Cypher queries for continuous monitoring
//! - Attaching a Log reaction to print results to console
//!
//! ## Running
//!
//! ```bash
//! # Start MS SQL Server in Docker
//! docker-compose up -d
//!
//! # Wait for SQL Server to be ready (check with docker-compose ps)
//! # Run setup script to create database and enable CDC
//! ./setup.sh
//!
//! # Run the example
//! cargo run
//!
//! # In another terminal, test with updates
//! ./test-updates.sh
//! ```

use anyhow::Result;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use drasi_bootstrap_mssql::MsSqlBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::LogReaction;
use drasi_source_mssql::MsSqlSource;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔════════════════════════════════════════════╗");
    println!("║   DrasiLib MS SQL Getting Started         ║");
    println!("╚════════════════════════════════════════════╝\n");

    // =========================================================================
    // Step 1: Create MS SQL Bootstrap Provider
    // =========================================================================
    let bootstrap_provider = MsSqlBootstrapProvider::builder()
        .with_host("localhost")
        .with_port(1433)
        .with_database("OrdersDB")
        .with_user("sa")
        .with_password("YourStrong!Passw0rd")
        .with_tables(vec!["dbo.Orders".to_string()])
        .build()?;

    println!("✓ Bootstrap provider created");

    // =========================================================================
    // Step 2: Create MS SQL Source
    // =========================================================================
    let mssql_source = MsSqlSource::builder("orders-source")
        .with_host("localhost")
        .with_port(1433)
        .with_database("OrdersDB")
        .with_user("sa")
        .with_password("YourStrong!Passw0rd")
        .with_table("dbo.Orders")
        .with_trust_server_certificate(true)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    println!("✓ MS SQL source created");

    // =========================================================================
    // Step 3: Define Queries
    // =========================================================================

    // Query 1: All Orders - Returns all orders with their details
    let all_orders_query = Query::cypher("all-orders")
        .query(
            r#"
            MATCH (o:Orders)
            RETURN o.OrderId AS order_id,
                   o.CustomerName AS customer_name,
                   o.Status AS status,
                   o.TotalAmount AS total_amount
        "#,
        )
        .from_source("orders-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    // Query 2: Pending Orders - Orders that are pending
    let pending_orders_query = Query::cypher("pending-orders")
        .query(
            r#"
            MATCH (o:Orders)
            WHERE o.Status = 'Pending'
            RETURN o.OrderId AS order_id,
                   o.CustomerName AS customer_name,
                   o.TotalAmount AS total_amount
        "#,
        )
        .from_source("orders-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    // Query 3: Large Orders - Orders over $1000
    let large_orders_query = Query::cypher("large-orders")
        .query(
            r#"
            MATCH (o:Orders)
            WHERE o.TotalAmount > 1000
            RETURN o.OrderId AS order_id,
                   o.CustomerName AS customer_name,
                   o.Status AS status,
                   o.TotalAmount AS total_amount
        "#,
        )
        .from_source("orders-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    println!("✓ Queries defined");

    // =========================================================================
    // Step 4: Create Log Reaction
    // =========================================================================
    use drasi_reaction_log::{QueryConfig, TemplateSpec};

    let log_reaction = LogReaction::builder("console-logger")
        .from_query("all-orders")
        .from_query("pending-orders")
        .from_query("large-orders")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec {
                template: "[{{query_name}}] + Order {{after.order_id}} ({{after.customer_name}}): {{after.status}} - ${{after.total_amount}}".to_string(),
                extension: (),
            }),
            updated: Some(TemplateSpec {
                template: "[{{query_name}}] ~ Order {{after.order_id}}: {{before.status}} -> {{after.status}}".to_string(),
                extension: (),
            }),
            deleted: Some(TemplateSpec {
                template: "[{{query_name}}] - Order {{before.order_id}} removed".to_string(),
                extension: (),
            }),
        })
        .build()?;

    println!("✓ Log reaction created");

    // =========================================================================
    // Step 5: Build DrasiLib
    // =========================================================================
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mssql-orders-monitor")
            .with_source(mssql_source)
            .with_query(all_orders_query)
            .with_query(pending_orders_query)
            .with_query(large_orders_query)
            .with_reaction(log_reaction)
            .build()
            .await?,
    );

    println!("✓ DrasiLib built");

    // =========================================================================
    // Step 6: Start Processing
    // =========================================================================
    core.start().await?;

    println!("✓ System started");

    // =========================================================================
    // Step 7: Start Results API Server
    // =========================================================================
    let api_core = core.clone();
    let results_api = Router::new()
        .route("/queries/{id}/results", get(get_query_results))
        .with_state(api_core);

    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("\n┌────────────────────────────────────────────┐");
    println!("│ MS SQL Orders Monitor Started!             │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Database: OrdersDB (localhost:1433)       │");
    println!("│ Table: dbo.Orders                          │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Queries:                                   │");
    println!("│   • all-orders     - All orders            │");
    println!("│   • pending-orders - Pending only          │");
    println!("│   • large-orders   - Amount > $1000        │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Results API: http://localhost:8080         │");
    println!("│   GET /queries/all-orders/results          │");
    println!("│   GET /queries/pending-orders/results      │");
    println!("│   GET /queries/large-orders/results        │");
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
