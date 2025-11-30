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

//! Minimal example demonstrating the instance-based plugin architecture.
//!
//! This example shows how to:
//! 1. Create a DrasiLib instance using the builder
//! 2. Create source and reaction plugin instances externally
//! 3. Add them to DrasiLib via add_source() and add_reaction()
//! 4. Start the system and let data flow
//!
//! The key insight: YOU create the plugin instances, not DrasiLib.
//! This eliminates registries, factories, and config-based creation.

use anyhow::Result;
use drasi_lib::{DrasiLib, api::Query};

// Import the plugin types and their configs directly
use drasi_plugin_mock::{MockSource, MockSourceConfig};
use drasi_plugin_log_reaction::{LogReaction, LogReactionConfig};
use drasi_lib::config::common::LogLevel;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("=== Drasi Instance-Based Plugin Architecture ===\n");

    // Step 1: Create a DrasiLib instance with just a query
    // Note: Sources and reactions are added AFTER build()
    println!("Step 1: Building DrasiLib with a query...");
    let drasi = DrasiLib::builder()
        .with_id("plugin-demo")
        .add_query(
            Query::cypher("counter-query")
                .query("MATCH (c:Counter) RETURN c.value as count, c.timestamp as ts")
                .from_source("mock-source")
                .auto_start(false)  // Don't auto-start - we'll start manually
                .build(),
        )
        .build()
        .await?;

    println!("✓ DrasiLib created\n");

    // Step 2: Create source configuration
    // This is the typed config that your plugin needs
    println!("Step 2: Creating MockSource plugin instance...");
    let mock_source_config = MockSourceConfig {
        data_type: "counter".to_string(),
        interval_ms: 2000,
    };

    // Step 3: Create the MockSource instance
    // YOU create it - not a registry or factory
    // Note: event_tx is no longer passed - DrasiLib injects it automatically when adding
    // Ownership is transferred to DrasiLib when you call add_source()
    let mock_source = MockSource::new("mock-source", mock_source_config)?;
    println!("✓ MockSource instance created\n");

    // Step 4: Add the source to DrasiLib
    // DrasiLib automatically injects the event channel during add_source()
    println!("Step 3: Adding source to DrasiLib...");
    drasi.add_source(mock_source).await?;
    println!("✓ Source added\n");

    // Step 5: Create reaction configuration
    println!("Step 4: Creating LogReaction plugin instance...");
    let log_reaction_config = LogReactionConfig {
        log_level: LogLevel::Info,
    };

    // Step 6: Create the LogReaction instance
    // Note: event_tx is no longer passed - DrasiLib injects it automatically when adding
    // Ownership is transferred to DrasiLib when you call add_reaction()
    let log_reaction = LogReaction::new(
        "log-reaction",
        vec!["counter-query".to_string()],
        log_reaction_config,
    );
    println!("✓ LogReaction instance created\n");

    // Step 7: Add the reaction to DrasiLib
    // DrasiLib automatically injects the event channel during add_reaction()
    println!("Step 5: Adding reaction to DrasiLib...");
    drasi.add_reaction(log_reaction).await?;
    println!("✓ Reaction added\n");

    // Step 8: Start DrasiLib (this starts all components)
    println!("Step 6: Starting DrasiLib...");
    drasi.start().await?;
    println!("✓ DrasiLib started\n");

    println!("=== System Running ===");
    println!("The MockSource generates counter data every 2 seconds.");
    println!("The LogReaction logs query results to the console.");
    println!("Press Ctrl+C to stop.\n");

    // Run for 10 seconds to show data flowing
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Stop gracefully
    println!("\nStopping DrasiLib...");
    drasi.stop().await?;
    println!("✓ DrasiLib stopped");

    println!("\n=== Instance-Based Architecture Benefits ===");
    println!("• No registries or factories needed");
    println!("• You control plugin instantiation");
    println!("• Type-safe configuration");
    println!("• No event_tx parameter needed - DrasiLib injects automatically");
    println!("• Easy to test with mock implementations");
    println!("• Clear dependency injection pattern");

    Ok(())
}
