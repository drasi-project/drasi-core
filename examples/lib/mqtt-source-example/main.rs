use anyhow::Result;
use drasi_lib::identity::PasswordIdentityProvider;
use drasi_lib::DrasiLib;
use drasi_lib::Query;
use drasi_reaction_log::LogReaction;
use drasi_reaction_log::{QueryConfig, TemplateSpec};
use drasi_source_mqtt::config::MqttSourceConfig;
use drasi_source_mqtt::MqttSource;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("╔════════════════════════════════════════════╗");
    println!("║     DrasiLib Temperature Monitor Example   ║");
    println!("╚════════════════════════════════════════════╝\n");

    // Add MqttSource configuration
    let source_config_yaml = r#"
host: "localhost"
port: 8883
topics:
  - topic: "sensors/temperature"
    qos: 1
  - topic: "building/+/+/+"
    qos: 0
topic_mappings:
  - pattern: "sensors/{type}"
    entity:
      label: "readings"
      id: "symbol"
    properties:
      mode: payload_as_field
      field_name: "{type}"
      inject:
      - type: "{type}"
  - pattern: "building/{floor}/{room}/{device}"
    entity:
      label: "DEVICE"
      id: "{room}:{device}"
    properties:
      mode: payload_as_field
      field_name: "reading"
      inject_id: "true"
      inject:
      - room: "{room}"
      - floor: "{floor}"
      - device: "{device}"
    nodes:
      - label: "FLOOR"
        id: "{floor}"
      - label: "ROOM"
        id: "{room}"
    relations:
      - label: "LOCATED_IN_FLOOR"
        from: "ROOM"
        to: "FLOOR"
        id: "{room}_located_in_{floor}"
      - label: "LOCATED_IN_ROOM"
        from: "DEVICE"
        to: "ROOM"
        id: "{device}_located_in_{room}"
event_channel_capacity: 20
keep_alive: 5
conn_timeout: 5
transport:
    mode: tls
    config:
        ca_path: "/var/certs-drasi/ca.crt"
        client_cert_path: "/var/certs-drasi/client.crt"
        client_key_path: "/var/certs-drasi/client.key"
"#;

    let source_config: MqttSourceConfig = serde_yaml::from_str(source_config_yaml)?;

    // Build MqttSource with the configuration
    let mqtt_source = match MqttSource::builder("mqtt-source")
        .with_config(source_config)
        .with_identity_provider(PasswordIdentityProvider::new(
            "drasi".to_string(),
            "drasi".to_string(),
        ))
        .build()
        .await
    {
        Ok(source) => source,
        Err(e) => {
            eprintln!("Failed to build MQTT source: {e}");
            return Err(e);
        }
    };

    // Define the query to read all device readings
    let all_readings_query = Query::cypher("floor_readings")
        .query(
            r#"
            MATCH (rd:DEVICE)-[:LOCATED_IN_ROOM]->(r:ROOM)-[:LOCATED_IN_FLOOR]->(f:FLOOR)
            WHERE rd.id = 'r1:d2' AND r.id = 'r1' AND f.id = 'f2'
            RETURN rd.reading as val,
                   rd.room as room,
                   rd.floor as floor,
                   rd.device as device,
                   rd.id as id,
                   r.id as room_id,
                   f.id as floor_id
        "#,
        )
        .from_source("mqtt-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    // Define a log reaction to print query results to console
    let default_template = QueryConfig {
        added: Some(TemplateSpec {
            template: "[{{query_name}}] + {{after.device}}: ${{after.val}} (Room: {{after.room}}, Floor: {{after.floor}}, Device: {{after.device}}, ID: {{after.id}}, Room ID: {{after.room_id}}, Floor ID: {{after.floor_id}})".to_string(),
            ..Default::default()
        }),
        updated: Some(TemplateSpec {
            template: "[{{query_name}}] ~ {{after.device}}: ${{before.val}} -> ${{after.val}} (Room: {{after.room}}, Floor: {{after.floor}}, Device: {{after.device}}, ID: {{after.id}}, Room ID: {{after.room_id}}, Floor ID: {{after.floor_id}})"
                .to_string(),
            ..Default::default()
        }),
        deleted: Some(TemplateSpec {
            template: "[{{query_name}}] - {{before.device}} removed".to_string(),
            ..Default::default()
        }),
    };

    // Build the log reaction
    let log_reaction = LogReaction::builder("console-logger")
        .from_query("floor_readings")
        .with_default_template(default_template)
        .build()?;

    // Build the DrasiLib core with the MQTT source, query, and reaction
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("monitor")
            .with_source(mqtt_source)
            .with_query(all_readings_query)
            .with_reaction(log_reaction)
            .build()
            .await?,
    );

    // Start the core
    println!("\n>>> Starting DrasiLib core...");
    core.start().await?;
    println!(">>> Core started successfully");

    let api_core = core.clone();
    let results_api = Router::new()
        .route("/queries/:id/results", get(get_query_results))
        .with_state(api_core);

    println!(">>> Starting Results API server on port 8080...");
    let api_handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
        println!(">>> Results API listening on http://0.0.0.0:8080");
        axum::serve(listener, results_api).await.unwrap();
    });

    println!("\n┌────────────────────────────────────────────┐");
    println!("│ Temperature Monitor Started!               │");
    println!("├────────────────────────────────────────────┤");
    println!("│ MQTT Source: localhost:9001                │");
    println!("│   Topic: sensors/temperature               │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Results API: http://localhost:8080         │");
    println!("│   GET /queries/all-readings/results        │");
    println!("├────────────────────────────────────────────┤");
    println!("│ Press Ctrl+C to stop                       │");
    println!("└────────────────────────────────────────────┘\n");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;

    println!("\n>>> Shutting down gracefully...");
    api_handle.abort();
    core.stop().await?;
    println!(">>> Shutdown complete.");

    Ok(())
}

/// Handler for GET /queries/:id/results
async fn get_query_results(
    State(core): State<Arc<DrasiLib>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<serde_json::Value>>, (axum::http::StatusCode, String)> {
    core.get_query_results(&id)
        .await
        .map(Json)
        .map_err(|e| (axum::http::StatusCode::NOT_FOUND, e.to_string()))
}
