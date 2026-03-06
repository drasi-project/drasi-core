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

//! Integration test for RabbitMQ reaction using testcontainers.

use anyhow::{anyhow, Result};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_rabbitmq::{
    ExchangeType, PublishSpec, QueryPublishConfig, RabbitMQReaction, RabbitMQReactionConfig,
};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use futures_util::StreamExt;
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, ExchangeDeclareOptions, QueueBindOptions,
        QueueDeclareOptions,
    },
    types::FieldTable,
    Connection, ConnectionProperties, Consumer, ExchangeKind,
};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::time::timeout;

async fn setup_rabbitmq() -> Result<(ContainerAsync<GenericImage>, String)> {
    let image = GenericImage::new("rabbitmq", "3.13-management")
        .with_exposed_port(5672.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Server startup complete"));

    let container = image.start().await?;
    let host = container.get_host().await?.to_string();
    let port = container.get_host_port_ipv4(5672.tcp()).await?;

    let connection_string = format!("amqp://guest:guest@{host}:{port}/%2f");
    Ok((container, connection_string))
}

async fn consume_next(consumer: &mut Consumer) -> Result<lapin::message::Delivery> {
    let delivery = timeout(Duration::from_secs(5), consumer.next())
        .await?
        .ok_or_else(|| anyhow!("No message received"))??;
    delivery.ack(BasicAckOptions::default()).await?;
    Ok(delivery)
}

#[tokio::test]
#[ignore]
async fn test_rabbitmq_reaction_end_to_end() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (_container, connection_string) = setup_rabbitmq().await?;

    let connection =
        Connection::connect(&connection_string, ConnectionProperties::default()).await?;
    let channel = connection.create_channel().await?;

    channel
        .exchange_declare(
            "drasi-test-exchange",
            ExchangeKind::Topic,
            ExchangeDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;

    let queue = channel
        .queue_declare(
            "",
            QueueDeclareOptions {
                exclusive: true,
                auto_delete: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await?;

    channel
        .queue_bind(
            queue.name().as_str(),
            "drasi-test-exchange",
            "entity.*",
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await?;

    let mut consumer = channel
        .basic_consume(
            queue.name().as_str(),
            "rabbitmq-reaction-test",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;

    let (source, handle) = ApplicationSource::new(
        "test-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )?;
    let query = Query::cypher("test-query")
        .query("MATCH (e:Entity) RETURN e.id AS id, e.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();

    let mut routes = HashMap::new();
    routes.insert(
        "test-query".to_string(),
        QueryPublishConfig {
            added: Some(PublishSpec {
                routing_key: "entity.created".to_string(),
                headers: HashMap::new(),
                body_template: Some(
                    r#"{"op":"ADD","id":{{after.id}},"name":{{json after.name}}}"#.to_string(),
                ),
            }),
            updated: Some(PublishSpec {
                routing_key: "entity.updated".to_string(),
                headers: HashMap::new(),
                body_template: Some(
                    r#"{"op":"UPDATE","before":{{json before}},"after":{{json after}}}"#
                        .to_string(),
                ),
            }),
            deleted: Some(PublishSpec {
                routing_key: "entity.deleted".to_string(),
                headers: HashMap::new(),
                body_template: Some(r#"{"op":"DELETE","id":{{before.id}}}"#.to_string()),
            }),
        },
    );

    let config = RabbitMQReactionConfig {
        connection_string: connection_string.clone(),
        exchange_name: "drasi-test-exchange".to_string(),
        exchange_type: ExchangeType::Topic,
        exchange_durable: true,
        message_persistent: false,
        tls_enabled: false,
        tls_cert_path: None,
        tls_key_path: None,
        query_configs: routes,
    };

    let reaction = RabbitMQReaction::new("test-reaction", vec!["test-query".to_string()], config)?;

    let core = DrasiLib::builder()
        .with_id("rabbitmq-test-core")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let props = PropertyMapBuilder::new()
        .with_integer("id", 1)
        .with_string("name", "Alice")
        .build();
    handle
        .send_node_insert("entity-1", vec!["Entity"], props)
        .await?;

    let add_delivery = consume_next(&mut consumer).await?;
    let add_body: Value = serde_json::from_slice(&add_delivery.data)?;
    assert_eq!(add_delivery.routing_key.as_str(), "entity.created");
    assert_eq!(add_body["op"], "ADD");
    assert_eq!(add_body["id"], 1);
    assert_eq!(add_body["name"], "Alice");

    let props = PropertyMapBuilder::new()
        .with_integer("id", 1)
        .with_string("name", "Alice Updated")
        .build();
    handle
        .send_node_update("entity-1", vec!["Entity"], props)
        .await?;

    let update_delivery = consume_next(&mut consumer).await?;
    let update_body: Value = serde_json::from_slice(&update_delivery.data)?;
    assert_eq!(update_delivery.routing_key.as_str(), "entity.updated");
    assert_eq!(update_body["op"], "UPDATE");
    assert!(update_body.get("before").is_some());
    assert!(update_body.get("after").is_some());

    handle.send_delete("entity-1", vec!["Entity"]).await?;

    let delete_delivery = consume_next(&mut consumer).await?;
    let delete_body: Value = serde_json::from_slice(&delete_delivery.data)?;
    assert_eq!(delete_delivery.routing_key.as_str(), "entity.deleted");
    assert_eq!(delete_body["op"], "DELETE");
    assert_eq!(delete_body["id"], 1);

    core.stop().await?;
    Ok(())
}
