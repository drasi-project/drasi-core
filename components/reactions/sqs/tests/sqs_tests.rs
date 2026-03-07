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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use aws_sdk_sqs::{types::QueueAttributeName, Client};
use aws_types::region::Region;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_sqs::{QueryConfig, SqsReaction, SqsReactionConfig, TemplateSpec};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::elasticmq::ElasticMq;
use uuid::Uuid;

fn setup_logger() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

#[test]
fn test_config_round_trip_serialization() {
    let mut attrs = HashMap::new();
    attrs.insert("sensor-id".to_string(), "{{after.id}}".to_string());
    let config = SqsReactionConfig {
        queue_url: "https://sqs.us-east-1.amazonaws.com/123/queue".to_string(),
        region: Some("us-east-1".to_string()),
        endpoint_url: Some("http://localhost:9324".to_string()),
        fifo_queue: true,
        message_group_id_template: Some("{{query_id}}".to_string()),
        access_key_id: None,
        secret_access_key: None,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec {
                body: "{\"id\":\"{{after.id}}\"}".to_string(),
                message_attributes: attrs,
            }),
            updated: None,
            deleted: None,
        }),
        routes: HashMap::new(),
    };

    let serialized = serde_json::to_string(&config).expect("serialization should work");
    let deserialized: SqsReactionConfig =
        serde_json::from_str(&serialized).expect("deserialization should work");
    assert_eq!(config, deserialized);
}

#[test]
fn test_template_spec_message_attribute_builder() {
    let template = TemplateSpec::new("{\"event\":\"{{operation}}\"}")
        .with_message_attribute("x-operation", "{{operation}}")
        .with_message_attribute("x-id", "{{after.id}}");

    assert_eq!(
        template.message_attributes.get("x-operation"),
        Some(&"{{operation}}".to_string())
    );
    assert_eq!(
        template.message_attributes.get("x-id"),
        Some(&"{{after.id}}".to_string())
    );
}

async fn make_sqs_client(endpoint: &str) -> Client {
    let creds = aws_credential_types::Credentials::new("test", "test", None, None, "test");
    let config = aws_config::from_env()
        .region(Region::new("us-east-1"))
        .endpoint_url(endpoint.to_string())
        .credentials_provider(creds)
        .load()
        .await;
    Client::new(&config)
}

async fn receive_single_message(
    client: &Client,
    queue_url: &str,
    timeout: Duration,
) -> Result<(serde_json::Value, HashMap<String, String>)> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        let response = client
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(1)
            .message_attribute_names("All")
            .send()
            .await?;

        if let Some(messages) = response.messages() {
            if let Some(message) = messages.first() {
                let body = message.body().unwrap_or("{}");
                let parsed_body: serde_json::Value = serde_json::from_str(body)?;
                let attrs = message
                    .message_attributes()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| {
                                v.string_value().map(|value| (k.clone(), value.to_string()))
                            })
                            .collect::<HashMap<_, _>>()
                    })
                    .unwrap_or_default();

                if let Some(receipt) = message.receipt_handle() {
                    client
                        .delete_message()
                        .queue_url(queue_url)
                        .receipt_handle(receipt)
                        .send()
                        .await?;
                }

                return Ok((parsed_body, attrs));
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    Err(anyhow!("timed out waiting for SQS message"))
}

#[tokio::test]
#[ignore]
async fn test_sqs_reaction_end_to_end() -> Result<()> {
    setup_logger();

    let container = ElasticMq::default().start().await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(9324).await?;
    let endpoint = format!("http://{host}:{port}");

    let client = make_sqs_client(&endpoint).await;
    let queue_name = format!("drasi-sqs-test-{}", Uuid::new_v4());
    let queue_url = client
        .create_queue()
        .queue_name(queue_name)
        .attributes(QueueAttributeName::VisibilityTimeout, "1")
        .send()
        .await?
        .queue_url()
        .ok_or_else(|| anyhow!("queue url missing"))?
        .to_string();

    let reaction = SqsReaction::builder("sqs-test-reaction")
        .with_query("sensor-query")
        .with_queue_url(queue_url.clone())
        .with_endpoint_url(endpoint.clone())
        .with_region("us-east-1")
        .with_credentials("test", "test")
        .with_default_template(QueryConfig {
            added: Some(
                TemplateSpec::new(
                    "{\"event\":\"add\",\"id\":\"{{after.id}}\",\"value\":{{after.value}}}",
                )
                .with_message_attribute("sensor-id", "{{after.id}}"),
            ),
            updated: Some(
                TemplateSpec::new(
                    "{\"event\":\"update\",\"id\":\"{{after.id}}\",\"before\":{{before.value}},\"after\":{{after.value}}}",
                )
                .with_message_attribute("sensor-id", "{{after.id}}"),
            ),
            deleted: Some(
                TemplateSpec::new("{\"event\":\"delete\",\"id\":\"{{before.id}}\"}")
                    .with_message_attribute("sensor-id", "{{before.id}}"),
            ),
        })
        .build()?;

    let (source, source_handle) = ApplicationSource::new(
        "app-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )?;
    let query = Query::cypher("sensor-query")
        .query("MATCH (s:Sensor) RETURN s.id AS id, s.value AS value")
        .from_source("app-source")
        .auto_start(true)
        .build();

    let core = DrasiLib::builder()
        .with_id("sqs-test-core")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;
    core.start().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    source_handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_integer("value", 42)
                .build(),
        )
        .await?;

    let (insert_body, insert_attrs) =
        receive_single_message(&client, &queue_url, Duration::from_secs(10)).await?;
    assert_eq!(insert_body["event"], "add");
    assert_eq!(insert_body["id"], "sensor-1");
    assert_eq!(insert_body["value"], 42);
    assert_eq!(
        insert_attrs.get("drasi-operation"),
        Some(&"ADD".to_string())
    );
    assert_eq!(
        insert_attrs.get("drasi-query-id"),
        Some(&"sensor-query".to_string())
    );
    assert_eq!(insert_attrs.get("sensor-id"), Some(&"sensor-1".to_string()));

    source_handle
        .send_node_update(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_integer("value", 99)
                .build(),
        )
        .await?;

    let (update_body, update_attrs) =
        receive_single_message(&client, &queue_url, Duration::from_secs(10)).await?;
    assert_eq!(update_body["event"], "update");
    assert_eq!(update_body["id"], "sensor-1");
    assert_eq!(update_body["before"], 42);
    assert_eq!(update_body["after"], 99);
    assert_eq!(
        update_attrs.get("drasi-operation"),
        Some(&"UPDATE".to_string())
    );
    assert_eq!(update_attrs.get("sensor-id"), Some(&"sensor-1".to_string()));

    source_handle
        .send_delete("sensor-1", vec!["Sensor"])
        .await?;

    let (delete_body, delete_attrs) =
        receive_single_message(&client, &queue_url, Duration::from_secs(10)).await?;
    assert_eq!(delete_body["event"], "delete");
    assert_eq!(delete_body["id"], "sensor-1");
    assert_eq!(
        delete_attrs.get("drasi-operation"),
        Some(&"DELETE".to_string())
    );
    assert_eq!(delete_attrs.get("sensor-id"), Some(&"sensor-1".to_string()));

    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_sqs_reaction_aggregation_query() -> Result<()> {
    setup_logger();

    let container = ElasticMq::default().start().await?;
    let host = container.get_host().await?;
    let port = container.get_host_port_ipv4(9324).await?;
    let endpoint = format!("http://{host}:{port}");

    let client = make_sqs_client(&endpoint).await;
    let queue_name = format!("drasi-sqs-agg-test-{}", Uuid::new_v4());
    let queue_url = client
        .create_queue()
        .queue_name(queue_name)
        .attributes(QueueAttributeName::VisibilityTimeout, "1")
        .send()
        .await?
        .queue_url()
        .ok_or_else(|| anyhow!("queue url missing"))?
        .to_string();

    let reaction = SqsReaction::builder("sqs-agg-reaction")
        .with_query("agg-query")
        .with_queue_url(queue_url.clone())
        .with_endpoint_url(endpoint.clone())
        .with_region("us-east-1")
        .with_credentials("test", "test")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "{\"event\":\"add\",\"total\":{{after.total}},\"total_value\":{{after.total_value}}}",
            )),
            updated: Some(TemplateSpec::new(
                "{\"event\":\"update\",\"total\":{{after.total}},\"total_value\":{{after.total_value}}}",
            )),
            deleted: None,
        })
        .build()?;

    let (source, source_handle) = ApplicationSource::new(
        "app-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )?;
    let query = Query::cypher("agg-query")
        .query("MATCH (s:Sensor) RETURN count(s) AS total, sum(s.value) AS total_value")
        .from_source("app-source")
        .auto_start(true)
        .build();

    let core = DrasiLib::builder()
        .with_id("sqs-agg-test-core")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;
    core.start().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Insert first sensor — aggregation should fire with total=1, total_value=10
    source_handle
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_integer("value", 10)
                .build(),
        )
        .await?;

    let (body1, attrs1) =
        receive_single_message(&client, &queue_url, Duration::from_secs(10)).await?;
    assert_eq!(body1["total"], 1, "first insert: total should be 1");
    assert_eq!(
        body1["total_value"], 10.0,
        "first insert: total_value should be 10"
    );
    assert_eq!(attrs1.get("drasi-query-id"), Some(&"agg-query".to_string()));

    // Insert second sensor — aggregation should update to total=2, total_value=30
    source_handle
        .send_node_insert(
            "sensor-2",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-2")
                .with_integer("value", 20)
                .build(),
        )
        .await?;

    let (body2, attrs2) =
        receive_single_message(&client, &queue_url, Duration::from_secs(10)).await?;
    assert_eq!(
        body2["event"], "update",
        "second insert should be an aggregation update"
    );
    assert_eq!(body2["total"], 2, "second insert: total should be 2");
    assert_eq!(
        body2["total_value"], 30.0,
        "second insert: total_value should be 30"
    );
    assert_eq!(
        attrs2.get("drasi-operation"),
        Some(&"UPDATE".to_string()),
        "aggregation should route as UPDATE operation"
    );

    // Delete a sensor — aggregation should update to total=1, total_value=20
    source_handle
        .send_delete("sensor-1", vec!["Sensor"])
        .await?;

    let (body3, attrs3) =
        receive_single_message(&client, &queue_url, Duration::from_secs(10)).await?;
    assert_eq!(
        body3["event"], "update",
        "delete should trigger aggregation update"
    );
    assert_eq!(body3["total"], 1, "after delete: total should be 1");
    assert_eq!(
        body3["total_value"], 20.0,
        "after delete: total_value should be 20"
    );
    assert_eq!(
        attrs3.get("drasi-operation"),
        Some(&"UPDATE".to_string()),
        "aggregation after delete should route as UPDATE"
    );

    core.stop().await?;
    Ok(())
}
