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

use anyhow::Result;
use axum::{response::Html, Router};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_sse::SseReaction;
use drasi_source_sui_deepbook::{SuiDeepBookSource, Transport, DEFAULT_DEEPBOOK_PACKAGE_ID};
use log::info;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let rpc_endpoint = std::env::var("SUI_RPC_URL")
        .unwrap_or_else(|_| "https://fullnode.mainnet.sui.io:443".to_string());
    let package_id = std::env::var("DEEPBOOK_PACKAGE_ID")
        .unwrap_or_else(|_| DEFAULT_DEEPBOOK_PACKAGE_ID.to_string());
    let transport = match std::env::var("SUI_TRANSPORT")
        .unwrap_or_else(|_| "grpc".to_string())
        .to_lowercase()
        .as_str()
    {
        "json-rpc" | "jsonrpc" | "json_rpc" => Transport::JsonRpc,
        _ => Transport::Grpc,
    };
    let sse_port: u16 = std::env::var("SSE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let dashboard_port: u16 = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);

    info!("Using transport: {:?}", transport);

    // ── Source ──
    let source = SuiDeepBookSource::builder("deepbook-mainnet")
        .with_transport(transport)
        .with_rpc_endpoint(rpc_endpoint.clone())
        .with_deepbook_package_id(package_id.clone())
        .with_poll_interval_ms(2_000)
        .with_start_from_now()
        .with_enable_pool_nodes(true)
        .with_enable_trader_nodes(true)
        .with_enable_order_nodes(true)
        .build()?;

    // ── Queries ──
    // 1. Live event feed – every DeepBook event as it arrives
    let event_feed = Query::cypher("event-feed")
        .query(
            r#"
            MATCH (e:DeepBookEvent)
            RETURN
              e.entity_id    AS id,
              e.event_name   AS event_name,
              e.pool_id      AS pool_id,
              e.timestamp_ms AS timestamp,
              e.sender       AS sender,
              e.payload      AS payload
            "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    // 2. Pool tracker – enrichment Pool nodes discovered from events
    let pool_tracker = Query::cypher("pool-tracker")
        .query(
            r#"
            MATCH (p:Pool)
            RETURN
              p.pool_id     AS pool_id,
              p.base_asset  AS base_asset,
              p.quote_asset AS quote_asset
            "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    // 3. Trader tracker – enrichment Trader nodes discovered from events
    let trader_tracker = Query::cypher("trader-tracker")
        .query(
            r#"
            MATCH (t:Trader)
            RETURN t.address AS address
            "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    // 4. Event type breakdown – aggregation by event name
    let event_breakdown = Query::cypher("event-breakdown")
        .query(
            r#"
            MATCH (e:DeepBookEvent)
            RETURN e.event_name AS event_name, count(e) AS cnt
            "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    // 5. Flash loan tracker – individual flash loan events
    let flash_loan_tracker = Query::cypher("flash-loan-tracker")
        .query(
            r#"
            MATCH (e:DeepBookEvent)
            WHERE e.event_name = 'FlashLoanBorrowed'
            RETURN e.entity_id AS id,
              e.pool_id        AS pool_id,
              e.sender         AS sender,
              e.timestamp_ms   AS timestamp,
              e.payload        AS payload
            "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    // 6. Price oracle – conversion rate updates from PriceAdded events
    let price_oracle = Query::cypher("price-oracle")
        .query(
            r#"
            MATCH (e:DeepBookEvent)
            WHERE e.event_name = 'PriceAdded'
            RETURN e.entity_id AS id,
              e.pool_id        AS pool_id,
              e.timestamp_ms   AS timestamp,
              e.payload        AS payload
            "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    // ── SSE Reaction ──
    let sse = SseReaction::builder("dashboard-sse")
        .with_port(sse_port)
        .with_query("event-feed")
        .with_query("pool-tracker")
        .with_query("trader-tracker")
        .with_query("event-breakdown")
        .with_query("flash-loan-tracker")
        .with_query("price-oracle")
        .build()?;

    // ── Drasi Core ──
    let core = DrasiLib::builder()
        .with_id("sui-deepbook-dashboard")
        .with_source(source)
        .with_query(event_feed)
        .with_query(pool_tracker)
        .with_query(trader_tracker)
        .with_query(event_breakdown)
        .with_query(flash_loan_tracker)
        .with_query(price_oracle)
        .with_reaction(sse)
        .build()
        .await?;

    core.start().await?;
    info!("Drasi core started");

    // ── Dashboard HTTP Server ──
    let app = Router::new().route("/", axum::routing::get(|| async { Html(DASHBOARD_HTML) }));
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{dashboard_port}")).await?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    print_banner(&rpc_endpoint, &package_id, sse_port, dashboard_port);

    // ── Wait for Ctrl+C ──
    tokio::signal::ctrl_c().await?;
    println!("\nShutting down…");
    core.stop().await?;
    Ok(())
}

fn print_banner(rpc: &str, pkg: &str, sse_port: u16, dash_port: u16) {
    let pkg_short = if pkg.len() > 14 {
        format!("{}…{}", &pkg[..8], &pkg[pkg.len() - 6..])
    } else {
        pkg.to_string()
    };
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  DeepBook DeFi Dashboard                                   ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  RPC:       {rpc}");
    println!("║  Package:   {pkg_short}");
    println!("║  SSE:       http://localhost:{sse_port}/events");
    println!("║  Dashboard: http://localhost:{dash_port}");
    println!("║  Enrichment: Pool + Trader + Order nodes enabled");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  Open the dashboard URL in your browser.                   ║");
    println!("║  Press Ctrl+C to stop.                                     ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
}
