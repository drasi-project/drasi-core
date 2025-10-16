// Basic Mock Source Configuration Example
// This example demonstrates how to configure a simple mock source
// using the new fluent builder API.

use drasi_server_core::{DrasiServerCore, Properties, Query, Reaction, Source};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    println!("=== Basic Mock Source Example ===");
    println!("Building configuration with fluent API...\n");

    // Build server using new fluent API
    let core = DrasiServerCore::builder()
        .with_id("basic-mock-example")
        // Mock source configuration with bootstrap provider
        .add_source(
            Source::mock("mock-sensor-data")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "sensor")
                        .with_int("interval_ms", 2000)
                        .with_int("initial_count", 5),
                )
                .with_script_bootstrap(vec![
                    "examples/data/sensor_small.jsonl".to_string()
                ])
                .auto_start(true)
                .build(),
        )
        // Query configuration
        .add_query(
            Query::cypher("sensor-monitor")
                .query("MATCH (n:SensorReading) RETURN n")
                .from_source("mock-sensor-data")
                .with_property("description", json!("Monitor all sensor readings"))
                .auto_start(true)
                .build(),
        )
        // Reaction configuration
        .add_reaction(
            Reaction::log("sensor-logger")
                .subscribe_to("sensor-monitor")
                .with_properties(
                    Properties::new()
                        .with_string("log_level", "info")
                        .with_string("format", "json"),
                )
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    println!("✓ Server configured successfully");
    println!("✓ Components:");
    println!("  - Source: mock-sensor-data (with script bootstrap)");
    println!("  - Query: sensor-monitor");
    println!("  - Reaction: sensor-logger\n");

    core.start().await?;
    println!("✓ Drasi Server Core started\n");
    println!("Press Ctrl+C to stop...");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("\nShutting down...");

    core.stop().await?;
    println!("✓ Shutdown complete");
    Ok(())
}
