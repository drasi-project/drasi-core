// Script File Bootstrap Source Configuration Example
// This example demonstrates how to configure a source with
// script_file bootstrap provider for initial data loading from JSONL files.

use drasi_server_core::{
    config::{
        DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, QueryLanguage,
        ReactionConfig, SourceConfig,
    },
    bootstrap::BootstrapProviderConfig,
};
use serde_json::json;
use std::collections::HashMap;

fn create_script_file_bootstrap_config() -> DrasiServerCoreConfig {
    // Server configuration
    let server = DrasiServerCoreSettings {
        host: "0.0.0.0".to_string(),
        port: 8080,
        log_level: "info".to_string(),
        max_connections: 1000,
        shutdown_timeout_seconds: 30,
        disable_persistence: false,
    };

    // Source with script_file bootstrap provider
    let mut source_properties = HashMap::new();
    source_properties.insert("data_type".to_string(), json!("person"));
    source_properties.insert("interval_ms".to_string(), json!(5000));
    source_properties.insert("initial_count".to_string(), json!(0));

    let user_source = SourceConfig {
        id: "user-data-source".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: source_properties,
        bootstrap_provider: Some(BootstrapProviderConfig::ScriptFile {
            file_paths: vec!["/data/users.jsonl".to_string()],
        }),
    };

    // Query with filtering
    let mut query_properties = HashMap::new();
    query_properties.insert("description".to_string(), json!("Monitor users over 25 years old"));

    let user_query = QueryConfig {
        id: "user-monitor".to_string(),
        query: "MATCH (n:Person) WHERE n.age > 25 RETURN n".to_string(),
        query_language: QueryLanguage::Cypher,
        sources: vec!["user-data-source".to_string()],
        auto_start: true,
        properties: query_properties,
        joins: None,
    };

    // Enhanced logging reaction
    let mut reaction_properties = HashMap::new();
    reaction_properties.insert("log_level".to_string(), json!("info"));
    reaction_properties.insert("format".to_string(), json!("json"));
    reaction_properties.insert("include_metadata".to_string(), json!(true));

    let webhook_reaction = ReactionConfig {
        id: "user-webhook".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["user-monitor".to_string()],
        auto_start: true,
        properties: reaction_properties,
    };

    // Complete configuration
    DrasiServerCoreConfig {
        server,
        sources: vec![user_source],
        queries: vec![user_query],
        reactions: vec![webhook_reaction],
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create the configuration
    let config = create_script_file_bootstrap_config();

    // Initialize and start the server core
    let runtime_config = std::sync::Arc::new(drasi_server_core::RuntimeConfig::from(config));
    let mut core = drasi_server_core::DrasiServerCore::new(runtime_config);

    core.initialize().await?;
    core.start().await?;

    println!("Drasi Server Core started with script_file bootstrap configuration");
    println!("Bootstrap data will be loaded from: /data/users.jsonl (JSONL format)");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("Shutting down...");

    core.shutdown().await?;
    Ok(())
}