// Core library for reactive data processing
use anyhow::Result;
use drasi_server_core::config::{QueryConfig, ReactionConfig, SourceConfig};
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig, RuntimeConfig};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
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
        "programmatic" => run_programmatic_mode().await,
        _ => {
            println!("Usage: {} [config|programmatic] [config_file]", args[0]);
            println!("  config:        Use YAML/JSON configuration (default)");
            println!("  programmatic:  Create configuration programmatically");
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

    // Load configuration from file
    let config = DrasiServerCoreConfig::load_from_file(&config_file)?;
    let runtime_config = Arc::new(RuntimeConfig::from(config));

    println!("Configuration loaded successfully");

    // Create and initialize the Drasi Server Core
    // This is the main integration point - just 3 lines to embed Drasi!
    let mut core = DrasiServerCore::new(runtime_config);

    println!("Initializing Drasi Server Core...");
    core.initialize().await?; // Set up internal components

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

async fn run_programmatic_mode() -> Result<()> {
    println!("=== Programmatic Integration Example ===");
    println!("Creating configuration programmatically...");

    // Create configuration programmatically instead of loading from file
    let config = create_programmatic_config();
    let runtime_config = Arc::new(RuntimeConfig::from(config));

    println!("Configuration created successfully");

    // Create and initialize the Drasi Server Core
    let mut core = DrasiServerCore::new(runtime_config);

    println!("Initializing Drasi Server Core...");
    core.initialize().await?;

    println!("Starting Drasi Server Core...");
    core.start().await?;

    println!("Drasi Server Core is running!");
    println!("The programmatically configured mock source generates sensor data every 1 second");
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

/// Example of creating configuration entirely in Rust code
/// This shows how to programmatically build all the components
fn create_programmatic_config() -> DrasiServerCoreConfig {
    use drasi_server_core::config::{DrasiServerCoreSettings, QueryLanguage};
    use serde_json::json;

    // Server configuration - same as YAML server section
    let server = DrasiServerCoreSettings {
        host: "0.0.0.0".to_string(),
        port: 8080,
        log_level: "info".to_string(),
        max_connections: 100,
        shutdown_timeout_seconds: 5,
        disable_persistence: false,
    };

    // Create a mock source for sensor data
    // This replaces the YAML sources section
    let mut source_properties = HashMap::new();
    source_properties.insert("data_type".to_string(), json!("sensor"));
    source_properties.insert("interval_ms".to_string(), json!(1000));

    let source = SourceConfig {
        id: "sensor-source".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: source_properties,
        bootstrap_provider: None,
    };

    // Create a query to filter sensor data
    // This replaces the YAML queries section
    let query = QueryConfig {
        id: "temperature-filter".to_string(),
        query: "MATCH (n:Sensor) WHERE n.temperature > 20 RETURN n".to_string(),
        query_language: QueryLanguage::Cypher, // Explicitly set language
        sources: vec!["sensor-source".to_string()],
        auto_start: true,
        properties: HashMap::new(),
        joins: None,
    };

    // Create a log reaction
    // This replaces the YAML reactions section
    let mut reaction_properties = HashMap::new();
    reaction_properties.insert("log_level".to_string(), json!("info"));

    let reaction = ReactionConfig {
        id: "temperature-logger".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["temperature-filter".to_string()],
        auto_start: true,
        properties: reaction_properties,
    };

    // Combine into full configuration
    DrasiServerCoreConfig {
        server,
        sources: vec![source],
        queries: vec![query],
        reactions: vec![reaction],
    }
}
