// Core library for reactive data processing
use anyhow::Result;
use drasi_lib::{DrasiLib, Properties, Query, Reaction, Source};
use std::env;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging to see what's happening
    env_logger::init();

    // Parse command line arguments
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("config");

    match mode {
        "config" => run_config_mode(&args).await,
        "builder" => run_builder_mode().await,
        _ => {
            println!("Usage: {} [config|builder]", args[0]);
            println!("  config:  Use YAML/JSON configuration (default)");
            println!("  builder: Use fluent builder API");
            Ok(())
        }
    }
}

async fn run_config_mode(args: &[String]) -> Result<()> {
    // Determine which configuration file to use
    let config_file = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "config/basic.yaml".to_string());

    println!("=== Configuration-Based Integration Example ===");
    println!("Loading configuration from: {}", config_file);

    // Load configuration from file and create ready-to-start server
    // This is the main integration point - just 2 lines to embed Drasi!
    let core = DrasiLib::from_config_file(&config_file).await?;

    println!("Configuration loaded and initialized successfully");

    println!("Starting Drasi Server Core...");
    core.start().await?; // Begin processing data

    println!("Drasi Server Core is running!");
    println!("The mock source will generate counter data every 2 seconds");
    println!("The query will filter values > 5 and log the results");
    println!("You should see data processing logs below...");
    println!("(Set RUST_LOG=debug to see detailed processing information)");
    println!("Running for 30 seconds...");

    // Let it run for 30 seconds to see the data flow
    sleep(Duration::from_secs(30)).await;

    // Graceful shutdown
    println!("Stopping Drasi Server Core...");
    core.stop().await?;

    println!("Example completed successfully!");
    Ok(())
}

async fn run_builder_mode() -> Result<()> {
    use serde_json::json;

    println!("=== Fluent Builder API Example ===");
    println!("Creating configuration using fluent builder API...");

    // Create server using fluent builder API
    // This is the NEW clean API - elegant and type-safe!
    let core = DrasiLib::builder()
        .with_id("example-server")
        // Add a mock source that generates sensor data
        .add_source(
            Source::mock("sensor-source")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "sensor")
                        .with_int("interval_ms", 1000),
                )
                .auto_start(true)
                .build(),
        )
        // Add a Cypher query to filter sensor data
        .add_query(
            Query::cypher("temperature-filter")
                .query("MATCH (n:Sensor) WHERE n.temperature > 20 RETURN n")
                .from_source("sensor-source")
                .auto_start(true)
                .build(),
        )
        // Add a log reaction to output results
        .add_reaction(
            Reaction::log("temperature-logger")
                .subscribe_to("temperature-filter")
                .with_property("log_level", json!("info"))
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    println!("Configuration created and initialized successfully");

    println!("Starting Drasi Server Core...");
    core.start().await?;

    println!("Drasi Server Core is running!");
    println!("The mock source generates sensor data every 1 second");
    println!("The query filters temperature > 20 and logs the results");
    println!("You should see data processing logs below...");
    println!("(Set RUST_LOG=debug to see detailed processing information)");
    println!("Running for 30 seconds...");

    // Let it run for 30 seconds to see the data flow
    sleep(Duration::from_secs(30)).await;

    // Graceful shutdown
    println!("Stopping Drasi Server Core...");
    core.stop().await?;

    println!("Example completed successfully!");
    Ok(())
}
