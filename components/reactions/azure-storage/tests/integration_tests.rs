// Copyright 2026 The Drasi Authors.
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

use azure_data_tables::clients::TableServiceClientBuilder;
use azure_data_tables::prelude::TableServiceClient;
use azure_storage::CloudLocation;
use azure_storage::StorageCredentials;
use azure_storage_blobs::prelude::{BlobServiceClient, ClientBuilder};
use azure_storage_queues::prelude::QueueServiceClient;
use azure_storage_queues::QueueServiceClientBuilder;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_azure_storage::{
    AzureStorageReaction, QueryConfig, StorageTarget, TemplateSpec,
};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use futures::StreamExt;
use serde_json::Value;
use serial_test::serial;
use std::collections::HashMap;
use std::time::Duration;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::GenericImage;
use tokio::time::sleep;

const AZURITE_ACCOUNT: &str = "devstoreaccount1";
// DevSkim: ignore DS137138; Azurite well-known development key used only for integration tests
const AZURITE_KEY: &str =
    "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll until `check` returns `Ok(true)` or the timeout is exceeded.
async fn poll_until<F, Fut>(check: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    loop {
        if check().await {
            return true;
        }
        if start.elapsed() >= POLL_TIMEOUT {
            return false;
        }
        sleep(POLL_INTERVAL).await;
    }
}

struct AzuriteRuntime {
    #[allow(dead_code)]
    container: testcontainers::ContainerAsync<GenericImage>,
    blob_port: u16,
    queue_port: u16,
    table_port: u16,
}

impl AzuriteRuntime {
    async fn start() -> Self {
        let image = GenericImage::new("mcr.microsoft.com/azure-storage/azurite", "latest")
            .with_exposed_port(ContainerPort::Tcp(10000))
            .with_exposed_port(ContainerPort::Tcp(10001))
            .with_exposed_port(ContainerPort::Tcp(10002))
            .with_wait_for(WaitFor::message_on_stdout(
                "Azurite Blob service is successfully listening",
            ))
            .with_wait_for(WaitFor::message_on_stdout(
                "Azurite Queue service is successfully listening",
            ))
            .with_wait_for(WaitFor::message_on_stdout(
                "Azurite Table service is successfully listening",
            ));

        let container = image.start().await.expect("azurite container should start");
        let blob_port = container
            .get_host_port_ipv4(10000)
            .await
            .expect("blob port should resolve");
        let queue_port = container
            .get_host_port_ipv4(10001)
            .await
            .expect("queue port should resolve");
        let table_port = container
            .get_host_port_ipv4(10002)
            .await
            .expect("table port should resolve");

        Self {
            container,
            blob_port,
            queue_port,
            table_port,
        }
    }

    fn credentials() -> StorageCredentials {
        StorageCredentials::access_key(AZURITE_ACCOUNT, AZURITE_KEY)
    }

    fn blob_client(&self) -> BlobServiceClient {
        ClientBuilder::new(AZURITE_ACCOUNT, Self::credentials())
            .cloud_location(CloudLocation::Custom {
                account: AZURITE_ACCOUNT.to_string(),
                uri: format!("http://127.0.0.1:{}/{}", self.blob_port, AZURITE_ACCOUNT),
            })
            .blob_service_client()
    }

    fn queue_client(&self) -> QueueServiceClient {
        QueueServiceClientBuilder::new(AZURITE_ACCOUNT, Self::credentials())
            .cloud_location(CloudLocation::Custom {
                account: AZURITE_ACCOUNT.to_string(),
                uri: format!("http://127.0.0.1:{}/{}", self.queue_port, AZURITE_ACCOUNT),
            })
            .build()
    }

    fn table_client(&self) -> TableServiceClient {
        TableServiceClientBuilder::new(AZURITE_ACCOUNT, Self::credentials())
            .cloud_location(CloudLocation::Custom {
                account: AZURITE_ACCOUNT.to_string(),
                uri: format!("http://127.0.0.1:{}/{}", self.table_port, AZURITE_ACCOUNT),
            })
            .build()
    }
}

async fn start_drasi_with_reaction(
    reaction: AzureStorageReaction,
    query_id: &str,
) -> anyhow::Result<(DrasiLib, drasi_source_application::ApplicationSourceHandle)> {
    let (source, handle) = ApplicationSource::new(
        "app-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )?;
    let query = Query::cypher(query_id)
        .query("MATCH (n:Sensor) RETURN n.id AS id, n.name AS name, n.region AS region, n.value AS value")
        .from_source("app-source")
        .auto_start(true)
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;
    // Brief pause to allow async subscription wiring to complete
    sleep(Duration::from_millis(500)).await;
    Ok((drasi, handle))
}

async fn emit_insert_update_delete(
    handle: &drasi_source_application::ApplicationSourceHandle,
) -> anyhow::Result<()> {
    handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_string("name", "Alpha")
                .with_string("region", "west")
                .with_float("value", 10.0)
                .build(),
        )
        .await?;

    handle
        .send_node_update(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_string("name", "Alpha Updated")
                .with_string("region", "west")
                .with_float("value", 12.5)
                .build(),
        )
        .await?;

    handle.send_delete("sensor-1", vec!["Sensor"]).await?;
    Ok(())
}

async fn read_blob_text(
    client: &BlobServiceClient,
    container: &str,
    blob: &str,
) -> anyhow::Result<String> {
    let blob_client = client.container_client(container).blob_client(blob);
    let mut stream = blob_client.get().into_stream();
    let mut buffer = Vec::new();
    while let Some(item) = stream.next().await {
        let mut body = item?.data;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            buffer.extend(chunk);
        }
    }
    Ok(String::from_utf8(buffer)?)
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_queue_reaction_insert_update_delete() {
    let azurite = AzuriteRuntime::start().await;
    let queue_name = "drasiqueueevents";
    azurite
        .queue_client()
        .queue_client(queue_name)
        .create()
        .await
        .expect("queue should be created");

    let reaction = AzureStorageReaction::builder("queue-reaction")
        .with_query("queue-query")
        .with_account_name(AZURITE_ACCOUNT)
        .with_access_key(AZURITE_KEY)
        .with_queue_endpoint(format!("http://127.0.0.1:{}", azurite.queue_port))
        .with_target(StorageTarget::Queue {
            queue_name: queue_name.to_string(),
        })
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                r#"{"op":"{{operation}}","name":"{{after.name}}"}"#,
            )),
            updated: Some(TemplateSpec::new(
                r#"{"op":"{{operation}}","name":"{{after.name}}","before":"{{before.name}}"}"#,
            )),
            deleted: Some(TemplateSpec::new(
                r#"{"op":"{{operation}}","name":"{{before.name}}"}"#,
            )),
        })
        .build()
        .expect("reaction should build");

    let (drasi, handle) = start_drasi_with_reaction(reaction, "queue-query")
        .await
        .expect("drasi should start");
    emit_insert_update_delete(&handle)
        .await
        .expect("events should be emitted");

    // Poll until at least 3 messages are available
    let queue_client = azurite.queue_client().queue_client(queue_name);
    let collected: std::sync::Arc<tokio::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let found = poll_until(|| {
        let qc = queue_client.clone();
        let collected = collected.clone();
        async move {
            if let Ok(response) = qc.get_messages().await {
                let mut c = collected.lock().await;
                for message in response.messages {
                    let text = message.message_text.clone();
                    c.push(text);
                    let _ = qc.pop_receipt_client(message).delete().await;
                }
                c.len() >= 3
            } else {
                false
            }
        }
    })
    .await;

    let collected = collected.lock().await;
    assert!(
        found,
        "expected at least 3 queue messages within timeout, got {}",
        collected.len()
    );

    let operations: Vec<String> = collected
        .iter()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .filter_map(|v| v.get("op").and_then(Value::as_str).map(str::to_string))
        .collect();

    assert!(
        operations.iter().any(|op| op == "ADD"),
        "ADD message not found: {operations:?}"
    );
    assert!(
        operations.iter().any(|op| op == "UPDATE"),
        "UPDATE message not found: {operations:?}"
    );
    assert!(
        operations.iter().any(|op| op == "DELETE"),
        "DELETE message not found: {operations:?}"
    );

    drasi.stop().await.expect("drasi should stop");
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_queue_reaction_aggregation() {
    let azurite = AzuriteRuntime::start().await;
    let queue_name = "drasiaggregationqueue";
    azurite
        .queue_client()
        .queue_client(queue_name)
        .create()
        .await
        .expect("queue should be created");

    let reaction = AzureStorageReaction::builder("agg-queue-reaction")
        .with_query("agg-query")
        .with_account_name(AZURITE_ACCOUNT)
        .with_access_key(AZURITE_KEY)
        .with_queue_endpoint(format!("http://127.0.0.1:{}", azurite.queue_port))
        .with_target(StorageTarget::Queue {
            queue_name: queue_name.to_string(),
        })
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                r#"{"op":"{{operation}}","total_value":{{after.total_value}}}"#,
            )),
            updated: Some(TemplateSpec::new(
                r#"{"op":"{{operation}}","total_value":{{after.total_value}}}"#,
            )),
            deleted: Some(TemplateSpec::new(r#"{"op":"{{operation}}"}"#)),
        })
        .build()
        .expect("reaction should build");

    let (source, handle) = ApplicationSource::new(
        "app-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )
    .expect("source should build");

    let query = Query::cypher("agg-query")
        .query("MATCH (n:Sensor) RETURN sum(n.value) AS total_value")
        .from_source("app-source")
        .auto_start(true)
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .expect("drasi should build");

    drasi.start().await.expect("drasi should start");
    sleep(Duration::from_millis(500)).await;

    // First insert produces an aggregation result (sum goes from nothing to 10)
    handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_float("value", 10.0)
                .build(),
        )
        .await
        .expect("insert 1 should succeed");

    // Second insert changes the aggregation (sum goes from 10 to 30)
    handle
        .send_node_insert(
            "sensor-2",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-2")
                .with_float("value", 20.0)
                .build(),
        )
        .await
        .expect("insert 2 should succeed");

    // Update a value (sum goes from 30 to 35)
    handle
        .send_node_update(
            "sensor-2",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-2")
                .with_float("value", 25.0)
                .build(),
        )
        .await
        .expect("update should succeed");

    let queue_client = azurite.queue_client().queue_client(queue_name);
    let collected: std::sync::Arc<tokio::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let found = poll_until(|| {
        let qc = queue_client.clone();
        let collected = collected.clone();
        async move {
            if let Ok(response) = qc.get_messages().await {
                let mut c = collected.lock().await;
                for message in response.messages {
                    let text = message.message_text.clone();
                    c.push(text);
                    let _ = qc.pop_receipt_client(message).delete().await;
                }
                c.len() >= 3
            } else {
                false
            }
        }
    })
    .await;

    let collected = collected.lock().await;
    assert!(
        found,
        "expected at least 3 aggregation messages within timeout, got {}",
        collected.len()
    );

    // All messages should be UPDATE operations (aggregation is treated as update)
    let operations: Vec<String> = collected
        .iter()
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .filter_map(|v| v.get("op").and_then(Value::as_str).map(str::to_string))
        .collect();

    assert!(
        operations.iter().all(|op| op == "UPDATE"),
        "all aggregation messages should be UPDATE operations: {operations:?}"
    );

    // Verify the last message has the correct aggregated total (10 + 25 = 35)
    let last_total: Option<f64> = collected
        .last()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("total_value").and_then(Value::as_f64));

    assert_eq!(
        last_total,
        Some(35.0),
        "final aggregation total should be 35.0, got {last_total:?}",
    );

    drasi.stop().await.expect("drasi should stop");
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_blob_reaction_insert_update_delete() {
    let azurite = AzuriteRuntime::start().await;
    let container_name = "drasi-blob-test";
    azurite
        .blob_client()
        .container_client(container_name)
        .create()
        .await
        .expect("container should be created");

    let blob_path_template = "{{#if after}}{{after.id}}{{else}}{{before.id}}{{/if}}.txt";
    let reaction = AzureStorageReaction::builder("blob-reaction")
        .with_query("blob-query")
        .with_account_name(AZURITE_ACCOUNT)
        .with_access_key(AZURITE_KEY)
        .with_blob_endpoint(format!("http://127.0.0.1:{}", azurite.blob_port))
        .with_target(StorageTarget::Blob {
            container_name: container_name.to_string(),
            blob_path_template: blob_path_template.to_string(),
            content_type: "text/plain".to_string(),
        })
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("ADD:{{after.name}}")),
            updated: Some(TemplateSpec::new("UPDATE:{{after.name}}")),
            deleted: Some(TemplateSpec::new("DELETE:{{before.name}}")),
        })
        .build()
        .expect("reaction should build");

    let (drasi, handle) = start_drasi_with_reaction(reaction, "blob-query")
        .await
        .expect("drasi should start");

    let blob_client = azurite.blob_client();

    // INSERT
    handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_string("name", "Alpha")
                .with_string("region", "west")
                .with_float("value", 10.0)
                .build(),
        )
        .await
        .expect("insert should succeed");

    let add_ok = poll_until(|| {
        let c = blob_client.clone();
        async move {
            read_blob_text(&c, container_name, "sensor-1.txt")
                .await
                .map(|t| t == "ADD:Alpha")
                .unwrap_or(false)
        }
    })
    .await;
    assert!(add_ok, "blob should contain 'ADD:Alpha' after insert");

    // UPDATE
    handle
        .send_node_update(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_string("name", "Alpha Updated")
                .with_string("region", "west")
                .with_float("value", 12.5)
                .build(),
        )
        .await
        .expect("update should succeed");

    let update_ok = poll_until(|| {
        let c = blob_client.clone();
        async move {
            read_blob_text(&c, container_name, "sensor-1.txt")
                .await
                .map(|t| t == "UPDATE:Alpha Updated")
                .unwrap_or(false)
        }
    })
    .await;
    assert!(
        update_ok,
        "blob should contain 'UPDATE:Alpha Updated' after update"
    );

    // DELETE
    handle
        .send_delete("sensor-1", vec!["Sensor"])
        .await
        .expect("delete should succeed");

    let delete_ok = poll_until(|| {
        let c = blob_client.clone();
        async move {
            read_blob_text(&c, container_name, "sensor-1.txt")
                .await
                .is_err()
        }
    })
    .await;
    assert!(delete_ok, "blob should not exist after delete");

    drasi.stop().await.expect("drasi should stop");
}

#[tokio::test]
#[ignore]
#[serial]
async fn test_table_reaction_insert_update_delete() {
    let azurite = AzuriteRuntime::start().await;
    let table_name = "drasitableevents";
    azurite
        .table_client()
        .table_client(table_name)
        .create()
        .await
        .expect("table should be created");

    let reaction = AzureStorageReaction::builder("table-reaction")
        .with_query("table-query")
        .with_account_name(AZURITE_ACCOUNT)
        .with_access_key(AZURITE_KEY)
        .with_table_endpoint(format!("http://127.0.0.1:{}", azurite.table_port))
        .with_target(StorageTarget::Table {
            table_name: table_name.to_string(),
            partition_key_template: "{{#if after}}{{after.region}}{{else}}{{before.region}}{{/if}}"
                .to_string(),
            row_key_template: "{{#if after}}{{after.id}}{{else}}{{before.id}}{{/if}}".to_string(),
        })
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                r#"{"name":"{{after.name}}","value":{{after.value}},"op":"{{operation}}"}"#,
            )),
            updated: Some(TemplateSpec::new(
                r#"{"name":"{{after.name}}","value":{{after.value}},"op":"{{operation}}"}"#,
            )),
            deleted: None,
        })
        .build()
        .expect("reaction should build");

    let (drasi, handle) = start_drasi_with_reaction(reaction, "table-query")
        .await
        .expect("drasi should start");

    let entity_client = azurite
        .table_client()
        .table_client(table_name)
        .partition_key_client("west")
        .entity_client("sensor-1");

    // INSERT
    handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_string("name", "Alpha")
                .with_string("region", "west")
                .with_float("value", 10.0)
                .build(),
        )
        .await
        .expect("insert should succeed");

    let add_ok = poll_until(|| {
        let ec = entity_client.clone();
        async move {
            ec.get::<Value>()
                .await
                .map(|r| {
                    r.entity
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| n == "Alpha")
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }
    })
    .await;
    assert!(
        add_ok,
        "table entity with name=Alpha should exist after add"
    );

    // UPDATE
    handle
        .send_node_update(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_string("name", "Alpha Updated")
                .with_string("region", "west")
                .with_float("value", 12.5)
                .build(),
        )
        .await
        .expect("update should succeed");

    let update_ok = poll_until(|| {
        let ec = entity_client.clone();
        async move {
            ec.get::<Value>()
                .await
                .map(|r| {
                    r.entity
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| n == "Alpha Updated")
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        }
    })
    .await;
    assert!(
        update_ok,
        "table entity name should be 'Alpha Updated' after update"
    );

    // DELETE
    handle
        .send_delete("sensor-1", vec!["Sensor"])
        .await
        .expect("delete should succeed");

    let delete_ok = poll_until(|| {
        let ec = entity_client.clone();
        async move { ec.get::<Value>().await.is_err() }
    })
    .await;
    assert!(delete_ok, "table entity should be deleted");

    drasi.stop().await.expect("drasi should stop");
}
