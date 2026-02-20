// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use drasi_bootstrap_sqlite::{SqliteBootstrapProvider, TableKeyConfig as BootstrapTableKeyConfig};
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::channels::ResultDiff;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::Source;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::{Subscription, SubscriptionOptions};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_sqlite::{RestApiConfig, SqliteSource, TableKeyConfig};
use reqwest::Client;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::sleep;

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    sleep(Duration::from_millis(50)).await;
    port
}

fn id_matches(value: &serde_json::Value, expected: i64) -> bool {
    value
        .as_i64()
        .map(|id| id == expected)
        .or_else(|| value.as_str().map(|id| id == expected.to_string()))
        .unwrap_or(false)
}

fn count_from_rows(rows: &[serde_json::Map<String, serde_json::Value>]) -> i64 {
    rows.first()
        .and_then(|row| row.get("count"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|count| count.parse::<i64>().ok()))
        })
        .unwrap_or_default()
}

async fn wait_for_diff<F>(subscription: &mut Subscription, description: &str, predicate: F)
where
    F: Fn(&ResultDiff) -> bool,
{
    for _ in 0..20 {
        if let Some(result) = subscription.recv().await {
            if result.results.iter().any(&predicate) {
                return;
            }
        }
    }

    panic!("timed out waiting for {description}");
}

async fn wait_for_query_results<F>(
    core: &Arc<DrasiLib>,
    query_id: &str,
    description: &str,
    predicate: F,
) where
    F: Fn(&[serde_json::Value]) -> bool,
{
    for _ in 0..20 {
        let rows = core.get_query_results(query_id).await.unwrap();
        if predicate(&rows) {
            return;
        }
        sleep(Duration::from_millis(200)).await;
    }

    panic!("timed out waiting for {description}");
}

#[tokio::test]
#[ignore]
async fn sqlite_handle_create_update_delete_flow() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("handle.db");

    let source = SqliteSource::builder("sqlite-handle-source")
        .with_path(db_path.to_string_lossy().to_string())
        .with_table_keys(vec![TableKeyConfig {
            table: "sensors".to_string(),
            key_columns: vec!["id".to_string()],
        }])
        .build()
        .unwrap();
    let handle = source.handle();

    let query = Query::cypher("sqlite-handle-query")
        .query("MATCH (s:sensors) RETURN s.id AS id, s.name AS name, s.temp AS temp")
        .from_source("sqlite-handle-source")
        .auto_start(true)
        .build();

    let (reaction, reaction_handle) = ApplicationReactionBuilder::new("sqlite-handle-reaction")
        .with_query("sqlite-handle-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("sqlite-handle-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = reaction_handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(1)))
        .await
        .unwrap();

    handle
        .execute("CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT, temp REAL)")
        .await
        .unwrap();

    handle
        .execute("INSERT INTO sensors(id, name, temp) VALUES (1, 'sensor-a', 31.5)")
        .await
        .unwrap();
    wait_for_diff(&mut subscription, "insert add diff", |diff| match diff {
        ResultDiff::Add { data } => data.get("name") == Some(&json!("sensor-a")),
        _ => false,
    })
    .await;

    handle
        .execute("UPDATE sensors SET name = 'sensor-a-updated', temp = 33.1 WHERE id = 1")
        .await
        .unwrap();
    wait_for_diff(&mut subscription, "update diff", |diff| match diff {
        ResultDiff::Update { after, .. } => {
            after.get("name") == Some(&json!("sensor-a-updated"))
                && id_matches(after.get("id").unwrap_or(&serde_json::Value::Null), 1)
        }
        _ => false,
    })
    .await;

    handle
        .execute("DELETE FROM sensors WHERE id = 1")
        .await
        .unwrap();
    wait_for_diff(&mut subscription, "delete diff", |diff| match diff {
        ResultDiff::Delete { data } => {
            data.get("name") == Some(&json!("sensor-a-updated"))
                && id_matches(data.get("id").unwrap_or(&serde_json::Value::Null), 1)
        }
        _ => false,
    })
    .await;

    core.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn sqlite_rest_crud_and_batch_flow() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("rest.db");
    let rest_port = find_available_port().await;

    let source = SqliteSource::builder("sqlite-rest-source")
        .with_path(db_path.to_string_lossy().to_string())
        .with_table_keys(vec![TableKeyConfig {
            table: "sensors".to_string(),
            key_columns: vec!["id".to_string()],
        }])
        .with_rest_api(RestApiConfig {
            host: "127.0.0.1".to_string(),
            port: rest_port,
        })
        .build()
        .unwrap();
    let handle = source.handle();

    let query = Query::cypher("sqlite-rest-query")
        .query("MATCH (s:sensors) RETURN s.id AS id, s.name AS name, s.temp AS temp")
        .from_source("sqlite-rest-source")
        .auto_start(true)
        .build();

    let (reaction, reaction_handle) = ApplicationReactionBuilder::new("sqlite-rest-reaction")
        .with_query("sqlite-rest-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("sqlite-rest-core")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(250)).await;

    handle
        .execute("CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT, temp REAL)")
        .await
        .unwrap();

    let mut subscription = reaction_handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(1)))
        .await
        .unwrap();
    let client = Client::new();
    let base = format!("http://127.0.0.1:{rest_port}");

    let insert_response = client
        .post(format!("{base}/api/tables/sensors"))
        .json(&json!({"id": 10, "name": "rest-insert", "temp": 40.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(insert_response.status(), 200);
    wait_for_diff(
        &mut subscription,
        "rest insert add diff",
        |diff| match diff {
            ResultDiff::Add { data } => data.get("name") == Some(&json!("rest-insert")),
            _ => false,
        },
    )
    .await;

    let update_response = client
        .put(format!("{base}/api/tables/sensors/10"))
        .json(&json!({"name": "rest-updated", "temp": 41.5}))
        .send()
        .await
        .unwrap();
    assert_eq!(update_response.status(), 200);
    wait_for_diff(&mut subscription, "rest update diff", |diff| match diff {
        ResultDiff::Update { after, .. } => after.get("name") == Some(&json!("rest-updated")),
        _ => false,
    })
    .await;

    let delete_response = client
        .delete(format!("{base}/api/tables/sensors/10"))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_response.status(), 200);
    wait_for_diff(&mut subscription, "rest delete diff", |diff| match diff {
        ResultDiff::Delete { data } => data.get("name") == Some(&json!("rest-updated")),
        _ => false,
    })
    .await;

    let failed_batch = client
        .post(format!("{base}/api/batch"))
        .json(&json!({
            "operations": [
                {"op": "insert", "table": "sensors", "data": {"id": 20, "name": "rollback-me", "temp": 50.0}},
                {"op": "update", "table": "sensors", "id": "20", "data": {"missing_column": 1}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(failed_batch.status(), 500);

    let rolled_back_rows = handle
        .query("SELECT COUNT(*) AS count FROM sensors WHERE id = 20")
        .await
        .unwrap();
    assert_eq!(count_from_rows(&rolled_back_rows), 0);

    let successful_batch = client
        .post(format!("{base}/api/batch"))
        .json(&json!({
            "operations": [
                {"op": "insert", "table": "sensors", "data": {"id": 30, "name": "batch-a", "temp": 60.0}},
                {"op": "insert", "table": "sensors", "data": {"id": 31, "name": "batch-b", "temp": 61.0}}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(successful_batch.status(), 200);

    wait_for_query_results(
        &core,
        "sqlite-rest-query",
        "batch rows in query results",
        |rows| {
            let has_30 = rows
                .iter()
                .any(|row| row.get("id").map(|id| id_matches(id, 30)).unwrap_or(false));
            let has_31 = rows
                .iter()
                .any(|row| row.get("id").map(|id| id_matches(id, 31)).unwrap_or(false));
            has_30 && has_31
        },
    )
    .await;

    core.stop().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn sqlite_bootstrap_loads_existing_rows() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("bootstrap.db");

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT, temp REAL);
            INSERT INTO sensors(id, name, temp) VALUES (1, 'bootstrap-sensor', 29.0);
            "#,
        )
        .unwrap();
    }

    let bootstrap_provider = SqliteBootstrapProvider::builder()
        .with_path(db_path.to_string_lossy().to_string())
        .with_tables(vec!["sensors".to_string()])
        .with_table_keys(vec![BootstrapTableKeyConfig {
            table: "sensors".to_string(),
            key_columns: vec!["id".to_string()],
        }])
        .build();

    let source = SqliteSource::builder("sqlite-bootstrap-source")
        .with_path(db_path.to_string_lossy().to_string())
        .with_table_keys(vec![TableKeyConfig {
            table: "sensors".to_string(),
            key_columns: vec!["id".to_string()],
        }])
        .with_bootstrap_provider(bootstrap_provider)
        .build()
        .unwrap();

    let settings = SourceSubscriptionSettings {
        source_id: "sqlite-bootstrap-source".to_string(),
        enable_bootstrap: true,
        query_id: "sqlite-bootstrap-query".to_string(),
        nodes: HashSet::from(["sensors".to_string()]),
        relations: HashSet::new(),
    };

    let response = source.subscribe(settings).await.unwrap();
    let mut bootstrap_rx = response
        .bootstrap_receiver
        .expect("missing bootstrap receiver");
    let bootstrap_event = tokio::time::timeout(Duration::from_secs(2), bootstrap_rx.recv())
        .await
        .expect("timed out waiting for bootstrap event")
        .expect("bootstrap channel closed");

    assert_eq!(bootstrap_event.source_id, "sqlite-bootstrap-source");
    match bootstrap_event.change {
        SourceChange::Insert {
            element: Element::Node { properties, .. },
        } => match properties.get("name") {
            Some(ElementValue::String(value)) => assert_eq!(value.as_ref(), "bootstrap-sensor"),
            other => panic!("unexpected name value: {other:?}"),
        },
        other => panic!("unexpected bootstrap change: {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn sqlite_multi_table_changes_flow_to_queries() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("multi-table.db");

    let source = SqliteSource::builder("sqlite-multi-source")
        .with_path(db_path.to_string_lossy().to_string())
        .with_table_keys(vec![
            TableKeyConfig {
                table: "sensors".to_string(),
                key_columns: vec!["id".to_string()],
            },
            TableKeyConfig {
                table: "devices".to_string(),
                key_columns: vec!["id".to_string()],
            },
        ])
        .build()
        .unwrap();
    let handle = source.handle();

    let sensors_query = Query::cypher("sqlite-sensors-query")
        .query("MATCH (s:sensors) RETURN s.id AS id, s.name AS name")
        .from_source("sqlite-multi-source")
        .auto_start(true)
        .build();
    let devices_query = Query::cypher("sqlite-devices-query")
        .query("MATCH (d:devices) RETURN d.id AS id, d.name AS name")
        .from_source("sqlite-multi-source")
        .auto_start(true)
        .build();

    let (reaction, reaction_handle) = ApplicationReactionBuilder::new("sqlite-multi-reaction")
        .with_queries(vec![
            "sqlite-sensors-query".to_string(),
            "sqlite-devices-query".to_string(),
        ])
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("sqlite-multi-core")
            .with_source(source)
            .with_query(sensors_query)
            .with_query(devices_query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(250)).await;

    let mut subscription = reaction_handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(1)))
        .await
        .unwrap();

    handle
        .execute_batch(
            r#"
            CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT);
            CREATE TABLE devices(id INTEGER PRIMARY KEY, name TEXT);
            "#,
        )
        .await
        .unwrap();
    handle
        .execute("INSERT INTO sensors(id, name) VALUES (1, 'sensor-one')")
        .await
        .unwrap();
    handle
        .execute("INSERT INTO devices(id, name) VALUES (9, 'device-nine')")
        .await
        .unwrap();

    let mut saw_sensor_query = false;
    let mut saw_device_query = false;
    for _ in 0..20 {
        if let Some(result) = subscription.recv().await {
            match result.query_id.as_str() {
                "sqlite-sensors-query" => {
                    saw_sensor_query = result.results.iter().any(|diff| match diff {
                        ResultDiff::Add { data } => data.get("name") == Some(&json!("sensor-one")),
                        _ => false,
                    });
                }
                "sqlite-devices-query" => {
                    saw_device_query = result.results.iter().any(|diff| match diff {
                        ResultDiff::Add { data } => data.get("name") == Some(&json!("device-nine")),
                        _ => false,
                    });
                }
                _ => {}
            }
        }

        if saw_sensor_query && saw_device_query {
            break;
        }
    }

    assert!(saw_sensor_query, "did not receive sensors query result");
    assert!(saw_device_query, "did not receive devices query result");

    core.stop().await.unwrap();
}
