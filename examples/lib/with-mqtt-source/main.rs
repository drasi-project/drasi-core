use drasi_source_mqtt::MQTTSource;
use anyhow::Result;
use drasi_reaction_log::{QueryConfig, TemplateSpec};
use drasi_lib::Query;
use drasi_reaction_log::LogReaction;
use drasi_lib::DrasiLib;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

#[tokio::main]
async fn main() -> Result<()> {

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    println!("╔════════════════════════════════════════════╗");
    println!("║     DrasiLib Temperature Monitor Example   ║");
    println!("╚════════════════════════════════════════════╝\n");


    // TODO: Add bootstrapper


    // Create MQTT source

    let mqtt_source = MQTTSource::builder("mqtt-source")
        .with_host("localhost")
        .with_port(1883)
        .with_topic("sensors/temperature")
        .with_qos(drasi_source_mqtt::model::QualityOfService::AtLeastOnce)
        .with_adaptive_max_batch_size(3)
        .with_adaptive_min_batch_size(1)
        .with_adaptive_max_wait_ms(1000)
        .build()?;

    let all_readings_query = Query::cypher("all-readings")
        .query(r#"
            MATCH (rd:readings)
            RETURN rd.symbol AS symbol,
                   rd.val AS value
        "#)
        .from_source("mqtt-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    
    
    let default_template = QueryConfig {
         added: Some(TemplateSpec{
             template: "[{{query_name}}] + {{after.symbol}}: ${{after.val}}".to_string(),
             ..Default::default()
         }),
         updated: Some(TemplateSpec{
             template: "[{{query_name}}] ~ {{after.symbol}}: ${{before.val}} -> ${{after.val}}".to_string(),
             ..Default::default()
         }),
         deleted: Some(TemplateSpec{
             template: "[{{query_name}}] - {{before.symbol}} removed".to_string(),
            ..Default::default()
         }),
     };

    let log_reaction = LogReaction::builder("console-logger")
        .from_query("all-readings")
        .with_default_template(default_template)
        .build()?;


    let core = Arc::new(
        DrasiLib::builder()
            .with_id("monitor")
            .with_source(mqtt_source)
            .with_query(all_readings_query)
            .with_reaction(log_reaction)
            .build()
            .await?
    );

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
    println!("│ MQTT Source: localhost:1883                │");
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