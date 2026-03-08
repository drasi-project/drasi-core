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
use chrono::{TimeZone, Utc};
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_sui_deepbook::{StartPosition, SuiDeepBookSource, DEFAULT_DEEPBOOK_PACKAGE_ID};
use std::time::Duration;

// ANSI colour helpers
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const MAGENTA: &str = "\x1b[35m";
const WHITE: &str = "\x1b[37m";

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let rpc_endpoint = std::env::var("SUI_RPC_URL")
        .unwrap_or_else(|_| "https://fullnode.mainnet.sui.io:443".to_string());
    let package_id = std::env::var("DEEPBOOK_PACKAGE_ID")
        .unwrap_or_else(|_| DEFAULT_DEEPBOOK_PACKAGE_ID.to_string());

    let source = SuiDeepBookSource::builder("deepbook-mainnet")
        .with_rpc_endpoint(rpc_endpoint.clone())
        .with_deepbook_package_id(package_id.clone())
        .with_poll_interval_ms(2_000)
        .with_start_position(StartPosition::Now)
        .with_enable_pool_nodes(true)
        .with_enable_trader_nodes(true)
        .with_enable_order_nodes(true)
        .build()?;

    let query = Query::cypher("deepbook-events")
        .query(
            r#"
            MATCH (e:DeepBookEvent)
            RETURN
              e.entity_id     AS entity_id,
              e.event_name    AS event_name,
              e.module        AS module,
              e.change_type   AS change_type,
              e.pool_id_short AS pool,
              e.sender_short  AS sender,
              e.timestamp_ms  AS timestamp_ms,
              e.order_id      AS order_id,
              e.payload       AS payload
        "#,
        )
        .from_source("deepbook-mainnet")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("deepbook-app")
        .with_query("deepbook-events")
        .build();

    let core = DrasiLib::builder()
        .with_id("sui-deepbook-example")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;

    print_banner(&rpc_endpoint, &package_id);

    let mut subscription = handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
        )
        .await?;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                println!("\n{DIM}Shutting down…{RESET}");
                break;
            }
            result = subscription.recv() => {
                if let Some(query_result) = result {
                    for diff in &query_result.results {
                        print_diff(diff);
                    }
                }
            }
        }
    }

    core.stop().await?;
    Ok(())
}

fn print_banner(rpc_endpoint: &str, package_id: &str) {
    let pkg_short = truncate_hex(package_id);
    println!();
    println!("{BOLD}{CYAN}╔══════════════════════════════════════════════════════════════╗{RESET}");
    println!("{BOLD}{CYAN}║{RESET}  {BOLD}Sui DeepBook Live Event Monitor{RESET}                             {BOLD}{CYAN}║{RESET}");
    println!("{BOLD}{CYAN}╠══════════════════════════════════════════════════════════════╣{RESET}");
    println!("{BOLD}{CYAN}║{RESET}  RPC:     {WHITE}{rpc_endpoint}{RESET}");
    println!("{BOLD}{CYAN}║{RESET}  Package: {WHITE}{pkg_short}{RESET}");
    println!("{BOLD}{CYAN}║{RESET}  Mode:    {GREEN}Live streaming (start from now){RESET}");
    println!("{BOLD}{CYAN}║{RESET}  Graph:   {GREEN}Pool + Trader + Order enrichment enabled{RESET}");
    println!("{BOLD}{CYAN}╠══════════════════════════════════════════════════════════════╣{RESET}");
    println!("{BOLD}{CYAN}║{RESET}  {DIM}Waiting for DeepBook events… (Ctrl+C to stop){RESET}");
    println!("{BOLD}{CYAN}╚══════════════════════════════════════════════════════════════╝{RESET}");
    println!();
}

fn print_diff(diff: &ResultDiff) {
    match diff {
        ResultDiff::Add { data } => {
            let (icon, color) = ("▶ ADD", GREEN);
            print_event(icon, color, data);
        }
        ResultDiff::Update { before, after, .. } => {
            let (icon, color) = ("⟳ UPD", YELLOW);
            print_event(icon, color, after);
            print_update_delta(before, after);
        }
        ResultDiff::Delete { data } => {
            let (icon, color) = ("✕ DEL", RED);
            print_event(icon, color, data);
        }
        _ => {}
    }
}

fn print_event(icon: &str, color: &str, data: &serde_json::Value) {
    let event_name = json_str(data, "event_name");
    let module = json_str(data, "module");
    let entity = json_str(data, "entity_id");
    let sender = json_str(data, "sender");
    let pool = json_str(data, "pool");
    let ts = format_timestamp(data);

    println!(
        "  {color}{BOLD}{icon}{RESET}  {BOLD}{event_name}{RESET} {DIM}({module}){RESET}"
    );
    println!(
        "        {DIM}entity:{RESET} {CYAN}{entity}{RESET}  \
         {DIM}sender:{RESET} {sender}  \
         {DIM}time:{RESET} {ts}"
    );

    if !pool.is_empty() {
        print!("        {DIM}pool:{RESET} {pool}");
    }

    let details = extract_payload_highlights(data);
    if !details.is_empty() {
        if !pool.is_empty() {
            print!("  ");
        } else {
            print!("        ");
        }
        print!("{MAGENTA}");
        print!("{details}");
        println!("{RESET}");
    } else if !pool.is_empty() {
        println!();
    }

    // Print the full payload JSON, pretty-printed and dimmed
    if let Some(payload) = data.get("payload") {
        if payload.is_object() && !payload.as_object().unwrap().is_empty() {
            if let Ok(pretty) = serde_json::to_string_pretty(payload) {
                println!("        {DIM}payload:{RESET}");
                for line in pretty.lines() {
                    println!("          {DIM}{line}{RESET}");
                }
            }
        }
    }

    println!();
}

fn print_update_delta(before: &serde_json::Value, after: &serde_json::Value) {
    let before_type = json_str(before, "change_type");
    let after_type = json_str(after, "change_type");
    if !before_type.is_empty() && before_type != after_type {
        println!(
            "        {DIM}transition:{RESET} {before_type} {YELLOW}→{RESET} {after_type}"
        );
        println!();
    }
}

fn extract_payload_highlights(data: &serde_json::Value) -> String {
    let payload = &data["payload"];
    if payload.is_null() || !payload.is_object() {
        return String::new();
    }

    let interesting: &[(&str, &str)] = &[
        ("price", "price"),
        ("size", "size"),
        ("quantity", "qty"),
        ("amount", "amount"),
        ("base_quantity", "base_qty"),
        ("quote_quantity", "quote_qty"),
        ("balance", "balance"),
        ("fee", "fee"),
        ("maker_fee", "maker_fee"),
        ("taker_fee", "taker_fee"),
        ("is_bid", "is_bid"),
        ("side", "side"),
        ("expire_timestamp", "expires"),
    ];

    let mut parts = Vec::new();
    for (key, label) in interesting {
        if let Some(val) = payload.get(key) {
            let display = match val {
                serde_json::Value::String(s) => {
                    if *key == "is_bid" {
                        if s == "true" { "BID".to_string() } else { "ASK".to_string() }
                    } else {
                        s.clone()
                    }
                }
                serde_json::Value::Bool(b) => {
                    if *key == "is_bid" {
                        if *b { "BID".to_string() } else { "ASK".to_string() }
                    } else {
                        b.to_string()
                    }
                }
                other => other.to_string(),
            };
            parts.push(format!("{label}={display}"));
        }
    }
    parts.join("  ")
}

fn format_timestamp(data: &serde_json::Value) -> String {
    data.get("timestamp_ms")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn json_str(data: &serde_json::Value, key: &str) -> String {
    data.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn truncate_hex(hex: &str) -> String {
    if hex.len() <= 14 {
        return hex.to_string();
    }
    let prefix = &hex[..8];
    let suffix = &hex[hex.len() - 6..];
    format!("{prefix}…{suffix}")
}
