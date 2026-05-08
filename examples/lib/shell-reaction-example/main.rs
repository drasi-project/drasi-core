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

//! # DrasiLib Shell Reaction Example
//!
//! This example demonstrates how to use the shell reaction to invoke a local
//! command for every query result change. Each change is rendered via a
//! Handlebars template and piped to the command's stdin.
//!
//! ## Running
//!
//! ```bash
//! cargo run --bin shell-reaction-example
//! ```
//!
//! ## Sending test events
//!
//! ```bash
//! # Add a sensor node
//! curl -X POST http://localhost:9000/sources/sensors/events \
//!   -H "Content-Type: application/json" \
//!   -d '{"operation":"upsert","element":{"type":"node","id":"sensor_01","labels":["sensors"],"properties":{"id":"sensor_01","temperature":22.5,"location":"room-a"}}}'
//!
//! # Update the temperature
//! curl -X POST http://localhost:9000/sources/sensors/events \
//!   -H "Content-Type: application/json" \
//!   -d '{"operation":"upsert","element":{"type":"node","id":"sensor_01","labels":["sensors"],"properties":{"id":"sensor_01","temperature":25.1,"location":"room-a"}}}'
//!
//! # Delete the sensor node
//! curl -X POST http://localhost:9000/sources/sensors/events \
//!   -H "Content-Type: application/json" \
//!   -d '{"operation":"delete","element":{"type":"node","id":"sensor_01","labels":["sensors"]}}'
//! ```

use anyhow::Result;
use std::sync::Arc;

use drasi_lib::{DrasiLib, Query};
use drasi_reaction_shell::{ ShellReactionBuilder };
use drasi_reaction_shell::config::{ TemplateSpec, QueryConfig, ShellExtension, ShellCommand };
use drasi_source_http::HttpSource;

use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔════════════════════════════════════════════╗");
    println!("║     DrasiLib Shell Reaction Example        ║");
    println!("╚════════════════════════════════════════════╝\n");

    // =========================================================================
    // Step 1: Create HTTP Source
    // =========================================================================
    // The HTTP source listens on port 9000 for incoming node change events.

    let http_source = HttpSource::builder("sensors")
        .with_host("0.0.0.0")
        .with_port(9000)
        .build()?;

    // =========================================================================
    // Step 2: Define Query
    // =========================================================================
    // Continuously monitors all `sensors` nodes and returns their properties.

    let sensor_query = Query::cypher("sensor-monitor")
        .query(r#"
            MATCH (s:sensors)
            RETURN s.id AS id,
                   s.temperature AS temperature,
                   s.location AS location
        "#)
        .from_source("sensors")
        .auto_start(true)
        .build();

    // =========================================================================
    // Step 3: Create Shell Reaction
    // =========================================================================
    // The shell reaction runs `cat` for every change, piping the rendered
    // template to its stdin. Replace `cat` with any script or binary you want
    // to invoke (e.g. a Python script, a notification tool, etc.).
    //
    // Templates use Handlebars syntax:
    //   - {{after.*}}  — new values (ADD / UPDATE)
    //   - {{before.*}} — old values (DELETE / UPDATE)

    let default_template = QueryConfig {
        // added: Some(TemplateSpec {
        //     template: "[ADD] sensor={{after.id}} temp={{after.temperature}} loc={{after.location}}".to_string(),
        //     extension: ShellExtension{
        //         envs: {
        //             let mut e = HashMap::new();
        //             e.insert("SENSOR_ID".to_string(), "from {{after.id}}".to_string());
        //             e
        //         },
        //     }
        // }),
        added: None,
        updated: Some(TemplateSpec {
            template: "[UPDATE] sensor={{after.id}} temp={{before.temperature}} -> {{after.temperature}}".to_string(),
            extension: ShellExtension::default(),
        }),
        deleted: Some(TemplateSpec {
            template: "[DELETE] sensor={{before.id}}".to_string(),
            extension: ShellExtension::default(),
        }),
    };

    let shell_reaction = ShellReactionBuilder::new("shell-logger")
        .with_query("sensor-monitor")
        .with_command("sensor-monitor", ShellCommand {
            executable: "/usr/bin/python3".to_string(),
            args: vec!["main.py".to_string()],
        })
        .with_default_template(default_template)
        .with_max_concurrent(3)
        .with_timeout_s(10)
        .build()?;

    // =========================================================================
    // Step 4: Build and Start DrasiLib
    // =========================================================================

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("shell-example")
            .with_source(http_source)
            .with_query(sensor_query)
            .with_reaction(shell_reaction)
            .build()
            .await?
    );

    core.start().await?;

    println!("\n┌────────────────────────────────────────────┐");
    println!("│ Shell Reaction Example Started!            │");
    println!("├────────────────────────────────────────────┤");
    println!("│ HTTP Source: http://localhost:9000         │");
    println!("│   POST /sources/sensors/events             │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Shell command: sleep 10 (simulates work)  │");
    println!("│ Watching query: sensor-monitor             │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Press Ctrl+C to stop                       │");
    println!("└────────────────────────────────────────────┘\n");

    tokio::signal::ctrl_c().await?;

    println!("\n>>> Shutting down...");
    core.stop().await?;
    println!(">>> Done.");

    Ok(())
}
