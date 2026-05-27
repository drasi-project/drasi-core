mod loki_helpers;

use std::collections::HashMap;
use std::time::Duration;

use drasi_lib::DrasiLib;
use drasi_lib::Query;
use drasi_reaction_loki::{LokiReaction, QueryConfig, TemplateSpec};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};

use loki_helpers::{setup_loki, wait_for_line_contains};

#[tokio::test]
#[ignore]
async fn test_loki_reaction_insert_update_delete() {
    let _ = env_logger::builder().is_test(true).try_init();

    let loki = setup_loki()
        .await
        .expect("Loki container should start successfully");
    let endpoint = loki.endpoint().to_string();

    let source_config = ApplicationSourceConfig {
        properties: HashMap::new(),
    };
    let (source, handle) = ApplicationSource::new("test-source", source_config)
        .expect("Application source should be created");

    let query = Query::cypher("sensor-query")
        .query(
            r#"
            MATCH (s:Sensor)
            RETURN s.id AS id, s.temperature AS temperature
            "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    let reaction = LokiReaction::builder("test-reaction")
        .with_query("sensor-query")
        .with_endpoint(endpoint.clone())
        .with_label("job", "drasi-test")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                r#"{"event":"ADD","id":"{{after.id}}","temperature":{{after.temperature}}}"#,
            )),
            updated: Some(TemplateSpec::new(r#"{"event":"UPDATE","id":"{{after.id}}","temperature_before":{{before.temperature}},"temperature_after":{{after.temperature}}}"#)),
            deleted: Some(TemplateSpec::new(r#"{"event":"DELETE","id":"{{before.id}}"}"#)),
        })
        .build()
        .expect("Loki reaction should build");

    let drasi = DrasiLib::builder()
        .with_id("loki-integration-test")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .expect("DrasiLib should build");

    drasi.start().await.expect("DrasiLib should start");
    tokio::time::sleep(Duration::from_secs(1)).await;

    handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_float("temperature", 25.0)
                .build(),
        )
        .await
        .expect("insert should succeed");

    wait_for_line_contains(
        &endpoint,
        r#"{job="drasi-test"}"#,
        r#""event":"ADD","id":"sensor-1""#,
        Duration::from_secs(15),
    )
    .await
    .expect("INSERT should be visible in Loki");

    handle
        .send_node_update(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_float("temperature", 31.5)
                .build(),
        )
        .await
        .expect("update should succeed");

    wait_for_line_contains(
        &endpoint,
        r#"{job="drasi-test"}"#,
        r#""event":"UPDATE","id":"sensor-1""#,
        Duration::from_secs(15),
    )
    .await
    .expect("UPDATE should be visible in Loki");

    handle
        .send_delete("sensor-1", vec!["Sensor"])
        .await
        .expect("delete should succeed");

    wait_for_line_contains(
        &endpoint,
        r#"{job="drasi-test"}"#,
        r#""event":"DELETE","id":"sensor-1""#,
        Duration::from_secs(15),
    )
    .await
    .expect("DELETE should be visible in Loki");

    drasi.stop().await.expect("DrasiLib should stop");
    loki.cleanup().await;
}
