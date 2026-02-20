use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_loki::{LokiReaction, QueryConfig, TemplateSpec};
use drasi_source_mock::{DataType, MockSource};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let loki_endpoint =
        std::env::var("LOKI_ENDPOINT").unwrap_or_else(|_| "http://localhost:3100".to_string());

    let bootstrap_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bootstrap_data.jsonl");
    let bootstrap_provider = ScriptFileBootstrapProvider::builder()
        .with_file(bootstrap_path.to_string_lossy().to_string())
        .build();

    let source = MockSource::builder("sensor-source")
        .with_data_type(DataType::sensor_reading(5))
        .with_interval_ms(2000)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("hot-sensors")
        .query(
            r#"
            MATCH (s:SensorReading)
            WHERE s.temperature > 24
            RETURN s.sensor_id AS sensor_id,
                   s.temperature AS temperature,
                   s.humidity AS humidity
            "#,
        )
        .from_source("sensor-source")
        .enable_bootstrap(true)
        .auto_start(true)
        .build();

    let reaction = LokiReaction::builder("loki-logger")
        .from_query("hot-sensors")
        .with_endpoint(&loki_endpoint)
        .with_label("job", "drasi-example")
        .with_label("source", "sensor-source")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                r#"{"event":"ADD","sensor":"{{after.sensor_id}}","temperature":{{after.temperature}}}"#,
            )),
            updated: Some(TemplateSpec::new(
                r#"{"event":"UPDATE","sensor":"{{after.sensor_id}}","temperature_before":{{before.temperature}},"temperature_after":{{after.temperature}}}"#,
            )),
            deleted: Some(TemplateSpec::new(
                r#"{"event":"DELETE","sensor":"{{before.sensor_id}}"}"#,
            )),
        })
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("loki-example")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;

    println!("----------------------------------------");
    println!("Loki example started");
    println!("----------------------------------------");
    println!("Loki endpoint: {loki_endpoint}");
    println!("Grafana: http://localhost:3000 (admin/admin)");
    println!("Query: hot-sensors");
    println!("Source: sensor-source (MockSource, 2s interval)");
    println!("Try querying Loki directly:");
    println!(
        "curl -G '{loki_endpoint}/loki/api/v1/query_range' --data-urlencode 'query={{job=\"drasi-example\"}}'"
    );
    println!("Dashboard UID: drasi-loki-overview");
    println!("----------------------------------------");
    println!("Press Ctrl+C to stop");
    println!("----------------------------------------");

    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
