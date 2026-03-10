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

//! # RIS Live Web Dashboard
//!
//! Connects to RIPE NCC RIS Live, maps BGP updates into graph data, and
//! serves a live-updating web dashboard at <http://localhost:3000>.

use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_ris_live::RisLiveSource;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

fn env_opt(name: &str) -> Option<String> {
    env::var(name).ok().and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

fn env_bool(name: &str, default: bool) -> bool {
    match env_opt(name) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        None => default,
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    env_opt(name)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_u16(name: &str, default: u16) -> u16 {
    env_opt(name)
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default)
}

/// Broadcast channel shared between the result processor and SSE handlers.
type EventTx = broadcast::Sender<String>;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let websocket_url = env_opt("RIS_WS_URL");
    let client_name =
        env_opt("RIS_CLIENT_NAME").unwrap_or_else(|| "drasi-ris-live-dashboard".to_string());
    let host = env_opt("RIS_HOST").unwrap_or_else(|| "rrc00".to_string());
    let message_type = env_opt("RIS_MESSAGE_TYPE").unwrap_or_else(|| "UPDATE".to_string());
    let prefix = env_opt("RIS_PREFIX");
    let include_peer_state = env_bool("RIS_INCLUDE_PEER_STATE", true);
    let reconnect_delay_secs = env_u64("RIS_RECONNECT_DELAY_SECS", 5);
    let dashboard_port = env_u16("DASHBOARD_PORT", 3000);

    // -- Source ---
    let mut source_builder = RisLiveSource::builder("ris-live")
        .with_client_name(client_name.clone())
        .with_host(host.clone())
        .with_message_type(message_type.clone())
        .with_require("announcements")
        .with_include_peer_state(include_peer_state)
        .with_reconnect_delay_secs(reconnect_delay_secs)
        .with_start_from_now()
        .with_auto_start(true);

    if let Some(url) = websocket_url.clone() {
        source_builder = source_builder.with_websocket_url(url);
    }
    if let Some(pf) = prefix.clone() {
        source_builder = source_builder.with_prefix(pf);
    }

    let source = source_builder.build()?;

    // -- Query --
    let query = Query::cypher("bgp-routes")
        .query(
            r#"
            MATCH (peer:Peer)-[r:ROUTES]->(p:Prefix)
            RETURN peer.peer_ip AS peer,
                   peer.peer_asn AS peer_asn,
                   p.prefix AS prefix,
                   r.next_hop AS next_hop,
                   r.origin_asn AS origin_asn,
                   r.path_length AS path_length
            "#,
        )
        .from_source("ris-live")
        .auto_start(true)
        .build();

    // -- Application Reaction (programmatic result capture) --
    let (reaction, handle) = ApplicationReactionBuilder::new("app-reaction")
        .with_queries(vec!["bgp-routes".to_string()])
        .build();

    // -- DrasiLib --
    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("ris-live-dashboard")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    drasi.start().await?;

    // -- Broadcast channel for SSE --
    let (event_tx, _) = broadcast::channel::<String>(512);
    let drasi_for_api = drasi.clone();

    // Spawn result processor that reads from ApplicationReaction and broadcasts.
    let event_tx_clone = event_tx.clone();
    tokio::spawn(async move {
        let mut subscription = match handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(1)),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                log::error!("Failed to subscribe to application reaction: {e}");
                return;
            }
        };

        loop {
            match subscription.recv().await {
                Some(result) => {
                    let diffs: Vec<serde_json::Value> = result
                        .results
                        .iter()
                        .filter_map(|d| diff_to_json(d))
                        .collect();
                    if diffs.is_empty() {
                        continue;
                    }
                    let payload = serde_json::json!({"query_id": result.query_id, "diffs": diffs});
                    let _ = event_tx_clone.send(payload.to_string());
                }
                None => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });

    // -- Axum web server --
    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/events", get(sse_handler))
        .route(
            "/api/routes",
            get(move || {
                let drasi = drasi_for_api.clone();
                async move {
                    match drasi.get_query_results("bgp-routes").await {
                        Ok(results) => Json(serde_json::json!({"routes": results})),
                        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
                    }
                }
            }),
        )
        .with_state(event_tx);

    println!();
    println!("╔════════════════════════════════════════════════════╗");
    println!("║       RIS Live – BGP Route Dashboard              ║");
    println!("╚════════════════════════════════════════════════════╝");
    println!();
    println!("  Dashboard:  http://localhost:{dashboard_port}"); // DevSkim: ignore DS162092
    println!("  SSE stream: http://localhost:{dashboard_port}/events"); // DevSkim: ignore DS162092
    println!("  REST API:   http://localhost:{dashboard_port}/api/routes"); // DevSkim: ignore DS162092
    println!();
    println!("  RIS host filter:    {host}");
    println!("  Message type:       {message_type}");
    if let Some(pf) = &prefix {
        println!("  Prefix filter:      {pf}");
    }
    println!(
        "  WebSocket URL:      {}",
        websocket_url.unwrap_or_else(|| "wss://ris-live.ripe.net/v1/ws/".to_string())
    );
    println!();
    println!("  Press Ctrl+C to stop");
    println!();

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{dashboard_port}")).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    println!("Shutting down…");
    drasi.stop().await?;
    println!("Stopped.");
    Ok(())
}

async fn serve_dashboard() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn sse_handler(
    axum::extract::State(tx): axum::extract::State<EventTx>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(data) => Some(Ok(Event::default().event("route-change").data(data))),
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    )
}

fn diff_to_json(diff: &ResultDiff) -> Option<serde_json::Value> {
    match diff {
        ResultDiff::Add { data } => Some(serde_json::json!({"ADD": {"data": data}})),
        ResultDiff::Update {
            before,
            after,
            data,
            ..
        } => Some(serde_json::json!({"UPDATE": {"before": before, "after": after, "data": data}})),
        ResultDiff::Delete { data } => Some(serde_json::json!({"DELETE": {"data": data}})),
        _ => None,
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.ok();
}
