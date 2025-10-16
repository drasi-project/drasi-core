// Script File Bootstrap Source Configuration Example
// This example demonstrates how to configure a source with
// script_file bootstrap provider for initial data loading from JSONL files.

use drasi_server_core::{DrasiServerCore, Properties, Query, Reaction, Source};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    println!("=== Script File Bootstrap Example ===");
    println!("Building configuration with script_file bootstrap provider...\n");

    // Build server using fluent API with script_file bootstrap
    let core = DrasiServerCore::builder()
        .with_id("file-bootstrap-example")
        // Source with script_file bootstrap provider
        .add_source(
            Source::mock("user-data-source")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "person")
                        .with_int("interval_ms", 5000)
                        .with_int("initial_count", 0),
                )
                .with_script_bootstrap(vec!["/data/users.jsonl".to_string()])
                .auto_start(true)
                .build(),
        )
        // Query with filtering
        .add_query(
            Query::cypher("user-monitor")
                .query("MATCH (n:Person) WHERE n.age > 25 RETURN n")
                .from_source("user-data-source")
                .with_property("description", json!("Monitor users over 25 years old"))
                .auto_start(true)
                .build(),
        )
        // Enhanced logging reaction
        .add_reaction(
            Reaction::log("user-webhook")
                .subscribe_to("user-monitor")
                .with_properties(
                    Properties::new()
                        .with_string("log_level", "info")
                        .with_string("format", "json")
                        .with_bool("include_metadata", true),
                )
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    println!("✓ Server configured successfully");
    println!("✓ Components:");
    println!("  - Source: user-data-source (with script_file bootstrap from /data/users.jsonl)");
    println!("  - Query: user-monitor (filter users > 25)");
    println!("  - Reaction: user-webhook (log results)\n");

    core.start().await?;
    println!("✓ Drasi Server Core started");
    println!("✓ Bootstrap data will be loaded from: /data/users.jsonl (JSONL format)\n");
    println!("Press Ctrl+C to stop...");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("\nShutting down...");

    core.stop().await?;
    println!("✓ Shutdown complete");
    Ok(())
}