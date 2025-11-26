// Platform Bootstrap Provider Example
//
// This example demonstrates how to configure a platform source with platform bootstrap provider
// using the builder API. The platform provider bootstraps data from a remote Query API service.

use drasi_lib::{
    api::{Query, Reaction, Source},
    DrasiLib,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    env_logger::init();

    // Build the server using the fluent API
    let core = DrasiLib::builder()
        .with_id("platform-bootstrap-example")
        .with_priority_queue_capacity(10000)
        .with_dispatch_buffer_capacity(1000)
        // Example 1: Platform source with inline query_api_url in bootstrap config
        .add_source(
            Source::platform("remote_platform_source")
                .with_property("redis_url", "redis://localhost:6379")
                .with_property("stream_key", "external-source:changes")
                .with_property("consumer_group", "drasi-core")
                .with_property("consumer_name", "consumer-1")
                .with_property("batch_size", 10)
                .with_property("block_ms", 5000)
                .with_platform_bootstrap(
                    "http://my-source-query-api:8080",
                    Some(300), // timeout_seconds
                )
                .build(),
        )
        // Example 2: Platform source with query_api_url in source properties (fallback)
        .add_source(
            Source::platform("remote_platform_source_alt")
                .with_property("redis_url", "redis://localhost:6379")
                .with_property("stream_key", "sensor-data:changes")
                .with_property("consumer_group", "drasi-core")
                .with_property("consumer_name", "consumer-1")
                .with_property("query_api_url", "http://sensor-query-api:8080")
                .with_platform_bootstrap_timeout(600) // Longer timeout for large datasets
                .build(),
        )
        // Example 3: Development/testing with short timeout
        .add_source(
            Source::platform("dev_platform_source")
                .with_property("redis_url", "redis://localhost:6379")
                .with_property("stream_key", "test-source:changes")
                .with_property("consumer_group", "dev-core")
                .with_property("consumer_name", "dev-consumer")
                .with_platform_bootstrap(
                    "http://localhost:8080",
                    Some(30), // Short timeout for development
                )
                .build(),
        )
        // This query will only receive Person nodes and KNOWS relations from bootstrap
        .add_query(
            Query::cypher("person_query")
                .query("MATCH (p:Person)-[k:KNOWS]->(p2:Person) RETURN p, k, p2")
                .from_source("remote_platform_source")
                .build(),
        )
        // This query receives all nodes and relations (no label filtering)
        .add_query(
            Query::cypher("all_data_query")
                .query("MATCH (n)-[r]->(m) RETURN n, r, m")
                .from_source("remote_platform_source")
                .build(),
        )
        .add_reaction(
            Reaction::log("platform_logger")
                .subscribe_to("person_query")
                .auto_start(true)
                .with_property("log_level", "info")
                .with_property("format", "json")
                .build(),
        )
        .build()
        .await?;

    println!("✓ Platform bootstrap example server initialized");
    println!("  - Server ID: platform-bootstrap-example");
    println!("  - Priority queue capacity: 10000");
    println!("  - Dispatch buffer capacity: 1000");
    println!("  - Sources: 3 platform sources with bootstrap");
    println!("  - Queries: 2 queries");
    println!("  - Reactions: 1 log reaction");

    // Start the server
    core.start().await?;
    println!("✓ Server started successfully");

    // Keep the server running
    println!("\nPress Ctrl+C to stop the server...");
    tokio::signal::ctrl_c().await?;

    println!("\n✓ Shutting down...");
    core.stop().await?;
    println!("✓ Server stopped");

    Ok(())
}
