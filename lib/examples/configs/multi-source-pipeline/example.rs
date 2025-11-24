// Multi-Source Pipeline Configuration Example
// This example demonstrates a complex pipeline with multiple sources,
// different bootstrap providers, and multiple queries and reactions.

use drasi_lib::{DrasiServerCore, Properties, Query, Reaction, Source};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    println!("=== Multi-Source Pipeline Example ===");
    println!("Building complex pipeline with fluent API...\n");

    // Build server with multiple sources, queries, and reactions using fluent API
    let core = DrasiServerCore::builder()
        .with_id("multi-source-pipeline")
        // Configure global capacity settings
        .with_priority_queue_capacity(20000) // Global default for queries/reactions
        .with_dispatch_buffer_capacity(2000) // Global default for sources/queries
        // Sensor stream with script_file bootstrap
        .add_source(
            Source::mock("sensor-stream")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "sensor")
                        .with_int("interval_ms", 1000)
                        .with_int("initial_count", 0),
                )
                .with_script_bootstrap(vec!["examples/data/sensor_data.jsonl".to_string()])
                .auto_start(true)
                .build(),
        )
        // User data with script_file bootstrap
        .add_source(
            Source::mock("user-data")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "person")
                        .with_int("interval_ms", 10000)
                        .with_int("initial_count", 0),
                )
                .with_script_bootstrap(vec!["/data/users.jsonl".to_string()])
                .auto_start(true)
                .build(),
        )
        // Event stream without bootstrap (noop provider will be used)
        .add_source(
            Source::mock("event-stream")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "event")
                        .with_int("interval_ms", 2000)
                        .with_int("initial_count", 10),
                )
                .auto_start(true)
                .build(),
        )
        // High temperature alert query with custom capacity
        .add_query(
            Query::cypher("high-temp-alert")
                .query("MATCH (s:SensorReading) WHERE s.temperature > 80 RETURN s")
                .from_source("sensor-stream")
                .with_property("description", json!("Alert on high temperature readings"))
                .with_property("threshold", json!(80))
                .with_priority_queue_capacity(50000) // Override global for high-volume query
                .auto_start(true)
                .build(),
        )
        // User activity query
        .add_query(
            Query::cypher("user-activity")
                .query("MATCH (u:Person)-[:TRIGGERED]->(e:Event) RETURN u, e")
                .from_sources(vec!["user-data", "event-stream"])
                .with_property("description", json!("Track user activity patterns"))
                .auto_start(true)
                .build(),
        )
        // Sensor summary query
        .add_query(
            Query::cypher("sensor-summary")
                .query("MATCH (s:SensorReading) RETURN s.location, avg(s.temperature) as avg_temp")
                .from_source("sensor-stream")
                .with_property("description", json!("Aggregate sensor readings by location"))
                .auto_start(true)
                .build(),
        )
        // Critical alert reaction with custom capacity
        .add_reaction(
            Reaction::log("critical-alert-webhook")
                .subscribe_to("high-temp-alert")
                .with_properties(
                    Properties::new()
                        .with_string("log_level", "warn")
                        .with_string("format", "json")
                        .with_bool("include_metadata", true),
                )
                .with_priority_queue_capacity(100000) // Override global for critical reliability
                .auto_start(true)
                .build(),
        )
        // Activity analytics reaction
        .add_reaction(
            Reaction::log("activity-analytics")
                .subscribe_to("user-activity")
                .with_properties(
                    Properties::new()
                        .with_string("log_level", "info")
                        .with_string("format", "json")
                        .with_int("batch_size", 100),
                )
                .auto_start(true)
                .build(),
        )
        // Dashboard feed reaction
        .add_reaction(
            Reaction::log("dashboard-feed")
                .subscribe_to("sensor-summary")
                .with_properties(
                    Properties::new()
                        .with_string("log_level", "debug")
                        .with_string("format", "json")
                        .with_int("update_interval", 5000),
                )
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    println!("✓ Server configured successfully");
    println!("✓ Configuration:");
    println!("  Global Settings:");
    println!("    - Priority queue capacity: 20000");
    println!("    - Dispatch buffer capacity: 2000");
    println!("\n  Sources:");
    println!("    - sensor-stream: Mock sensors with script_file bootstrap");
    println!("    - user-data: Mock users with script_file bootstrap");
    println!("    - event-stream: Mock events (no bootstrap)");
    println!("\n  Queries:");
    println!("    - high-temp-alert: Monitor sensors > 80° (capacity: 50000)");
    println!("    - user-activity: Track user-event relationships (capacity: 20000 default)");
    println!("    - sensor-summary: Aggregate readings by location (capacity: 20000 default)");
    println!("\n  Reactions:");
    println!("    - critical-alert-webhook: High temp alerts (capacity: 100000)");
    println!("    - activity-analytics: User activity data (capacity: 20000 default)");
    println!("    - dashboard-feed: Sensor summaries (capacity: 20000 default)\n");

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