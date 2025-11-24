// Platform Reaction Example
//
// This example demonstrates how to configure a platform reaction to publish
// query results to Redis Streams in Dapr CloudEvent format using the builder API.

use drasi_lib::{
    api::{Query, Reaction, Source},
    DrasiServerCore,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::init();

    // Build the server using the fluent API
    let core = DrasiServerCore::builder()
        .with_id("platform-reaction-example")
        .with_priority_queue_capacity(10000)
        .with_dispatch_buffer_capacity(1000)
        // Data source - using mock for demonstration
        .add_source(
            Source::mock("sensor-data")
                .auto_start(true)
                .with_property("data_type", "sensor")
                .with_property("interval_ms", 2000)
                .build(),
        )
        // Continuous query
        .add_query(
            Query::cypher("high-temperature-alert")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s")
                .from_source("sensor-data")
                .auto_start(true)
                .build(),
        )
        // Platform reaction - publishes to Redis Stream
        .add_reaction(
            Reaction::platform("platform-publisher")
                .subscribe_to("high-temperature-alert")
                .auto_start(true)
                // Required: Redis connection URL
                .with_property("redis_url", "redis://localhost:6379")
                // Optional: Pub/sub name for Dapr CloudEvent (default: "drasi-pubsub")
                .with_property("pubsub_name", "drasi-pubsub")
                // Optional: Source name for CloudEvent (default: "drasi-core")
                .with_property("source_name", "drasi-core")
                // Optional: Maximum stream length for trimming (default: unlimited)
                .with_property("max_stream_length", 10000)
                // Optional: Emit control events for lifecycle (default: true)
                .with_property("emit_control_events", true)
                .build(),
        )
        .build()
        .await?;

    println!("✓ Platform reaction example server initialized");
    println!("  - Server ID: platform-reaction-example");
    println!("  - Priority queue capacity: 10000");
    println!("  - Dispatch buffer capacity: 1000");
    println!("  - Source: mock sensor data");
    println!("  - Query: high-temperature-alert");
    println!("  - Reaction: platform publisher to Redis");
    println!("\nStream Naming Convention:");
    println!("  Results published to: high-temperature-alert-results");
    println!("\nCloudEvent Format:");
    println!("  - datacontenttype: application/json");
    println!("  - pubsubname: drasi-pubsub");
    println!("  - source: drasi-core");
    println!("  - topic: high-temperature-alert-results");
    println!("  - type: com.dapr.event.sent");

    // Start the server
    core.start().await?;
    println!("\n✓ Server started successfully");
    println!("  Monitoring for high temperature alerts (> 75°)...");

    // Keep the server running
    println!("\nPress Ctrl+C to stop the server...");
    tokio::signal::ctrl_c().await?;

    println!("\n✓ Shutting down...");
    core.stop().await?;
    println!("✓ Server stopped");

    Ok(())
}
