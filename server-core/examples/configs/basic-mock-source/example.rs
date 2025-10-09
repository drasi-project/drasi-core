// Basic Mock Source Configuration Example
// This example demonstrates how to configure a simple mock source
// using Rust code instead of YAML/JSON configuration files.

use drasi_server_core::{
    config::{
        DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, QueryLanguage,
        ReactionConfig, SourceConfig,
    },
    bootstrap::BootstrapProviderConfig,
};
use serde_json::json;
use std::collections::HashMap;

fn create_basic_mock_source_config() -> DrasiServerCoreConfig {
    // Server configuration
    let server = DrasiServerCoreSettings {
        host: "0.0.0.0".to_string(),
        port: 8080,
        log_level: "info".to_string(),
        max_connections: 1000,
        shutdown_timeout_seconds: 30,
        disable_persistence: false,
    };

    // Mock source configuration with bootstrap provider
    let mut source_properties = HashMap::new();
    source_properties.insert("data_type".to_string(), json!("sensor"));
    source_properties.insert("interval_ms".to_string(), json!(2000));
    source_properties.insert("initial_count".to_string(), json!(5));

    let mock_source = SourceConfig {
        id: "mock-sensor-data".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: source_properties,
        bootstrap_provider: Some(BootstrapProviderConfig::ScriptFile {
            file_paths: vec!["examples/data/sensor_small.jsonl".to_string()],
        }),
    };

    // Query configuration
    let mut query_properties = HashMap::new();
    query_properties.insert("description".to_string(), json!("Monitor all sensor readings"));

    let sensor_query = QueryConfig {
        id: "sensor-monitor".to_string(),
        query: "MATCH (n:SensorReading) RETURN n".to_string(),
        query_language: QueryLanguage::Cypher,
        sources: vec!["mock-sensor-data".to_string()],
        auto_start: true,
        properties: query_properties,
        joins: None,
    };

    // Reaction configuration
    let mut reaction_properties = HashMap::new();
    reaction_properties.insert("log_level".to_string(), json!("info"));
    reaction_properties.insert("format".to_string(), json!("json"));

    let log_reaction = ReactionConfig {
        id: "sensor-logger".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["sensor-monitor".to_string()],
        auto_start: true,
        properties: reaction_properties,
    };

    // Complete configuration
    DrasiServerCoreConfig {
        server,
        sources: vec![mock_source],
        queries: vec![sensor_query],
        reactions: vec![log_reaction],
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create the configuration
    let config = create_basic_mock_source_config();

    // Initialize and start the server core
    let runtime_config = std::sync::Arc::new(drasi_server_core::RuntimeConfig::from(config));
    let mut core = drasi_server_core::DrasiServerCore::new(runtime_config);

    core.initialize().await?;
    core.start().await?;

    println!("Drasi Server Core started with basic mock source configuration");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("Shutting down...");

    core.shutdown().await?;
    Ok(())
}