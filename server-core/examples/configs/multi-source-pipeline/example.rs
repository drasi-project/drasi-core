// Multi-Source Pipeline Configuration Example
// This example demonstrates a complex pipeline with multiple sources,
// different bootstrap providers, and multiple queries and reactions.

use drasi_server_core::{
    config::{
        DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, QueryLanguage,
        ReactionConfig, SourceConfig,
    },
    bootstrap::BootstrapProviderConfig,
};
use serde_json::json;
use std::collections::HashMap;

fn create_multi_source_pipeline_config() -> DrasiServerCoreConfig {
    // Enhanced server configuration
    let server = DrasiServerCoreSettings {
        host: "0.0.0.0".to_string(),
        port: 8080,
        log_level: "info".to_string(),
        max_connections: 2000,
        shutdown_timeout_seconds: 60,
        disable_persistence: false,
    };

    // Sensor stream with script_file bootstrap
    let mut sensor_properties = HashMap::new();
    sensor_properties.insert("data_type".to_string(), json!("sensor"));
    sensor_properties.insert("interval_ms".to_string(), json!(1000));
    sensor_properties.insert("initial_count".to_string(), json!(0));

    let sensor_source = SourceConfig {
        id: "sensor-stream".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: sensor_properties,
        bootstrap_provider: Some(BootstrapProviderConfig::ScriptFile {
            file_paths: vec!["examples/data/sensor_data.jsonl".to_string()],
        }),
    };

    // User data with script_file bootstrap
    let mut user_properties = HashMap::new();
    user_properties.insert("data_type".to_string(), json!("person"));
    user_properties.insert("interval_ms".to_string(), json!(10000));
    user_properties.insert("initial_count".to_string(), json!(0));

    let user_source = SourceConfig {
        id: "user-data".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: user_properties,
        bootstrap_provider: Some(BootstrapProviderConfig::ScriptFile {
            file_paths: vec!["/data/users.jsonl".to_string()],
        }),
    };

    // Event stream without bootstrap (noop provider will be used)
    let mut event_properties = HashMap::new();
    event_properties.insert("data_type".to_string(), json!("event"));
    event_properties.insert("interval_ms".to_string(), json!(2000));
    event_properties.insert("initial_count".to_string(), json!(10));

    let event_source = SourceConfig {
        id: "event-stream".to_string(),
        source_type: "mock".to_string(),
        auto_start: true,
        properties: event_properties,
        bootstrap_provider: None, // Will use noop provider
    };

    // High temperature alert query
    let mut temp_alert_props = HashMap::new();
    temp_alert_props.insert("description".to_string(), json!("Alert on high temperature readings"));
    temp_alert_props.insert("threshold".to_string(), json!(80));

    let high_temp_query = QueryConfig {
        id: "high-temp-alert".to_string(),
        query: "MATCH (s:SensorReading) WHERE s.temperature > 80 RETURN s".to_string(),
        query_language: QueryLanguage::Cypher,
        sources: vec!["sensor-stream".to_string()],
        auto_start: true,
        properties: temp_alert_props,
        joins: None,
    };

    // User activity query
    let mut activity_props = HashMap::new();
    activity_props.insert("description".to_string(), json!("Track user activity patterns"));

    let user_activity_query = QueryConfig {
        id: "user-activity".to_string(),
        query: "MATCH (u:Person)-[:TRIGGERED]->(e:Event) RETURN u, e".to_string(),
        query_language: QueryLanguage::Cypher,
        sources: vec!["user-data".to_string(), "event-stream".to_string()],
        auto_start: true,
        properties: activity_props,
        joins: None,
    };

    // Sensor summary query
    let mut summary_props = HashMap::new();
    summary_props.insert("description".to_string(), json!("Aggregate sensor readings by location"));

    let sensor_summary_query = QueryConfig {
        id: "sensor-summary".to_string(),
        query: "MATCH (s:SensorReading) RETURN s.location, avg(s.temperature) as avg_temp".to_string(),
        query_language: QueryLanguage::Cypher,
        sources: vec!["sensor-stream".to_string()],
        auto_start: true,
        properties: summary_props,
        joins: None,
    };

    // Critical alert reaction
    let mut alert_props = HashMap::new();
    alert_props.insert("log_level".to_string(), json!("warn"));
    alert_props.insert("format".to_string(), json!("json"));
    alert_props.insert("include_metadata".to_string(), json!(true));

    let critical_alert_reaction = ReactionConfig {
        id: "critical-alert-webhook".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["high-temp-alert".to_string()],
        auto_start: true,
        properties: alert_props,
    };

    // Activity analytics reaction
    let mut analytics_props = HashMap::new();
    analytics_props.insert("log_level".to_string(), json!("info"));
    analytics_props.insert("format".to_string(), json!("json"));
    analytics_props.insert("batch_size".to_string(), json!(100));

    let activity_analytics_reaction = ReactionConfig {
        id: "activity-analytics".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["user-activity".to_string()],
        auto_start: true,
        properties: analytics_props,
    };

    // Dashboard feed reaction
    let mut dashboard_props = HashMap::new();
    dashboard_props.insert("log_level".to_string(), json!("debug"));
    dashboard_props.insert("format".to_string(), json!("json"));
    dashboard_props.insert("update_interval".to_string(), json!(5000));

    let dashboard_feed_reaction = ReactionConfig {
        id: "dashboard-feed".to_string(),
        reaction_type: "internal.log".to_string(),
        queries: vec!["sensor-summary".to_string()],
        auto_start: true,
        properties: dashboard_props,
    };

    // Complete configuration
    DrasiServerCoreConfig {
        server,
        sources: vec![sensor_source, user_source, event_source],
        queries: vec![high_temp_query, user_activity_query, sensor_summary_query],
        reactions: vec![critical_alert_reaction, activity_analytics_reaction, dashboard_feed_reaction],
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create the configuration
    let config = create_multi_source_pipeline_config();

    // Initialize and start the server core
    let runtime_config = std::sync::Arc::new(drasi_server_core::RuntimeConfig::from(config));
    let mut core = drasi_server_core::DrasiServerCore::new(runtime_config);

    core.initialize().await?;
    core.start().await?;

    println!("Drasi Server Core started with multi-source pipeline configuration");
    println!("Sources:");
    println!("  - sensor-stream: Mock sensors with script_file bootstrap (50 initial readings)");
    println!("  - user-data: Mock users with script_file bootstrap from /data/users.jsonl");
    println!("  - event-stream: Mock events with no bootstrap");
    println!("Queries:");
    println!("  - high-temp-alert: Monitor sensors > 80°");
    println!("  - user-activity: Track user-event relationships");
    println!("  - sensor-summary: Aggregate readings by location");
    println!("Reactions:");
    println!("  - critical-alert-webhook: High temp alerts");
    println!("  - activity-analytics: User activity data");
    println!("  - dashboard-feed: Sensor summaries");

    // Keep running until interrupted
    tokio::signal::ctrl_c().await?;
    println!("Shutting down...");

    core.shutdown().await?;
    Ok(())
}