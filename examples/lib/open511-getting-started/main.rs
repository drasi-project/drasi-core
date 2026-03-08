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

//! Open511 getting started example.

use anyhow::Result;
use drasi_bootstrap_open511::Open511BootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
use drasi_source_open511::Open511Source;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔══════════════════════════════════════════════════════╗");
    println!("║       Open511 Road Events — Drasi Live Monitor      ║");
    println!("╚══════════════════════════════════════════════════════╝");
    println!();
    println!("Source:  DriveBC Open511 API");
    println!("Filter:  ACTIVE events, all severities");
    println!("Poll:    every 60s (full sweep every 10th cycle)");
    println!();

    let bootstrap_provider = Open511BootstrapProvider::builder()
        .with_base_url("https://api.open511.gov.bc.ca")
        .with_status_filter("ACTIVE")
        .build()?;

    let open511_source = Open511Source::builder("road-events")
        .with_base_url("https://api.open511.gov.bc.ca")
        .with_poll_interval_secs(60)
        .with_full_sweep_interval(10)
        .with_status_filter("ACTIVE")
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("road-incidents")
        .query(
            r#"
            MATCH (e:RoadEvent)-[:AFFECTS_ROAD]->(r:Road)
            WHERE e.status = 'ACTIVE'
            RETURN e.id AS event_id,
                   e.headline AS headline,
                   e.event_type AS event_type,
                   e.severity AS severity,
                   e.description AS description,
                   e.created AS created,
                   e.updated AS last_updated,
                   r.name AS road_name,
                   r.from AS road_from,
                   r.to AS road_to,
                   r.direction AS direction,
                   r.state AS road_state,
                   r.delay AS delay
        "#,
        )
        .from_source("road-events")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    // ANSI color codes for terminal output
    let red = "\x1b[31m";
    let green = "\x1b[32m";
    let yellow = "\x1b[33m";
    let cyan = "\x1b[36m";
    let bold = "\x1b[1m";
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";

    let template = QueryConfig {
        added: Some(TemplateSpec::new(&format!(
            "\n{red}{bold}🚨 ─── NEW ROAD EVENT ─────────────────────────────────────{reset}\n\
             {cyan}│ 🛣️  {{{{after.road_name}}}} ({{{{after.direction}}}}) — {bold}{{{{after.road_state}}}}{reset}\n\
             {cyan}│{reset}     From: {{{{after.road_from}}}}  →  To: {{{{after.road_to}}}}\n\
             {cyan}│{reset}\n\
             {cyan}│{reset} 📋 {bold}{{{{after.event_type}}}}{reset} — {{{{after.severity}}}}\n\
             {cyan}│{reset}    {{{{after.headline}}}}\n\
             {cyan}│{reset}    {dim}{{{{after.description}}}}{reset}\n\
             {cyan}│{reset}\n\
             {cyan}│{reset} ⏱️  Delay: {{{{after.delay}}}}\n\
             {cyan}│{reset} 📅 Created: {{{{after.created}}}}  Updated: {{{{after.last_updated}}}}\n\
             {cyan}│{reset} 🆔 {dim}{{{{after.event_id}}}}{reset}\n\
             {dim}└───────────────────────────────────────────────────────────{reset}",
        ))),
        updated: Some(TemplateSpec::new(&format!(
            "\n{yellow}{bold}🔄 ─── EVENT UPDATED ──────────────────────────────────────{reset}\n\
             {cyan}│ 🛣️  {{{{after.road_name}}}} ({{{{after.direction}}}}) — {bold}{{{{before.road_state}}}} → {{{{after.road_state}}}}{reset}\n\
             {cyan}│{reset}     From: {{{{after.road_from}}}}  →  To: {{{{after.road_to}}}}\n\
             {cyan}│{reset}\n\
             {cyan}│{reset} 📋 {bold}{{{{after.event_type}}}}{reset} — {{{{before.severity}}}} → {bold}{{{{after.severity}}}}{reset}\n\
             {cyan}│{reset}    {{{{after.headline}}}}\n\
             {cyan}│{reset}    {dim}{{{{after.description}}}}{reset}\n\
             {cyan}│{reset}\n\
             {cyan}│{reset} ⏱️  Delay: {{{{after.delay}}}}\n\
             {cyan}│{reset} 📅 Updated: {{{{after.last_updated}}}}\n\
             {cyan}│{reset} 🆔 {dim}{{{{after.event_id}}}}{reset}\n\
             {dim}└───────────────────────────────────────────────────────────{reset}",
        ))),
        deleted: Some(TemplateSpec::new(&format!(
            "\n{green}{bold}✅ ─── EVENT CLEARED ──────────────────────────────────────{reset}\n\
             {cyan}│ 🛣️  {{{{before.road_name}}}} ({{{{before.direction}}}})\n\
             {cyan}│{reset} 📋 {{{{before.event_type}}}} — {{{{before.severity}}}}\n\
             {cyan}│{reset}    {{{{before.headline}}}}\n\
             {cyan}│{reset} 🆔 {dim}{{{{before.event_id}}}}{reset}\n\
             {dim}└───────────────────────────────────────────────────────────{reset}",
        ))),
    };

    let reaction = LogReaction::builder("open511-log")
        .from_query("road-incidents")
        .with_default_template(template)
        .build()?;

    let core = DrasiLib::builder()
        .with_id("open511-example")
        .with_source(open511_source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;

    println!("────────────────────────────────────────────────────────");
    println!("  ✓ Live monitor active — watching for road events");
    println!("  ✓ Bootstrap loading current events from API...");
    println!("  ✓ Press Ctrl+C to stop");
    println!("────────────────────────────────────────────────────────");
    tokio::signal::ctrl_c().await?;

    println!();
    println!("Shutting down...");
    core.stop().await?;
    println!("Stopped.");

    Ok(())
}
