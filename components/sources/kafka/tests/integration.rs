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

//! Integration tests for the Kafka source.
//!
//! These tests require Docker to be running and available.
//! Run with: `cargo test -p drasi-source-kafka --test integration -- --ignored`

#![allow(clippy::unwrap_used)]

use drasi_bootstrap_kafka::KafkaBootstrapProvider;
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query, Source};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_kafka::KafkaSource;
use rdkafka::admin::{AdminClient, AdminOptions, NewTopic, TopicReplication};
use rdkafka::client::DefaultClientContext;
use rdkafka::error::RDKafkaErrorCode;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::kafka::apache;
use tokio::time::sleep;

/// Start a Kafka container and return the bootstrap servers address.
async fn start_kafka() -> (testcontainers::ContainerAsync<apache::Kafka>, String) {
    let kafka_node = apache::Kafka::default().start().await.unwrap();
    let port = kafka_node
        .get_host_port_ipv4(apache::KAFKA_PORT)
        .await
        .unwrap();
    let bootstrap_servers = format!("127.0.0.1:{port}");
    (kafka_node, bootstrap_servers)
}

/// Create a Kafka producer for test message publishing.
fn create_producer(bootstrap_servers: &str) -> FutureProducer {
    ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .create::<FutureProducer>()
        .expect("Failed to create Kafka FutureProducer")
}

/// Ensure test topic exists before starting the source.
async fn ensure_topic(bootstrap_servers: &str, topic: &str, partitions: i32) {
    let admin: AdminClient<DefaultClientContext> = ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .create()
        .expect("Failed to create Kafka admin client");

    let new_topic = NewTopic::new(topic, partitions, TopicReplication::Fixed(1));
    let result = admin
        .create_topics(&[new_topic], &AdminOptions::new())
        .await
        .expect("Failed to create topic");

    for topic_result in result {
        if let Err((_, code)) = topic_result {
            assert_eq!(
                code,
                RDKafkaErrorCode::TopicAlreadyExists,
                "Failed creating topic '{topic}': {code:?}"
            );
        }
    }
}

/// Test that Kafka messages are consumed and flow through the query engine.
#[tokio::test]
#[ignore] // Requires Docker
async fn test_kafka_source_detects_changes() {
    let _ = env_logger::try_init();

    let (_kafka_container, bootstrap_servers) = start_kafka().await;
    let topic = "test-orders";
    ensure_topic(&bootstrap_servers, topic, 3).await;

    // Create source
    let source = KafkaSource::builder("orders_kafka")
        .bootstrap_servers(&bootstrap_servers)
        .topic(topic)
        .group_id("drasi-test")
        .node_label("Order")
        .build()
        .unwrap();

    // Create query
    let query = Query::cypher("q1")
        .query("MATCH (o:Order) WHERE o.total > 50 RETURN o.id AS id, o.customer AS customer, o.total AS total")
        .from_source("orders_kafka")
        .build();

    // Create reaction
    let (reaction, handle) = ApplicationReactionBuilder::new("r1")
        .with_query("q1")
        .build();

    // Build and start DrasiLib
    let core = Arc::new(
        DrasiLib::builder()
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();

    // Create subscription to receive diffs
    let mut subscription = handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(10)),
        )
        .await
        .unwrap();

    // Give source time to connect and assign partitions (5s for slow CI runners)
    sleep(Duration::from_secs(5)).await;

    // Create producer and send messages
    let producer = create_producer(&bootstrap_servers);

    // INSERT: produce a message that should match the query (total > 50)
    producer
        .send(
            FutureRecord::to(topic)
                .key("order-1")
                .payload(r#"{"id":"order-1","customer":"Alice","total":150,"status":"pending"}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce message");

    // Wait for the diff
    let result = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
        .await
        .expect("Timed out waiting for result");

    let query_result = result.expect("Expected query result");
    assert!(!query_result.results.is_empty(), "Expected results");

    // Find the Add result
    let add = query_result
        .results
        .iter()
        .find(|r| matches!(r, ResultDiff::Add { .. }))
        .expect("Expected an Add result for order-1");
    if let ResultDiff::Add { data, .. } = add {
        assert_eq!(data["id"], "order-1");
        assert_eq!(data["customer"], "Alice");
    }

    // INSERT another message that does NOT match (total <= 50)
    producer
        .send(
            FutureRecord::to(topic)
                .key("order-2")
                .payload(r#"{"id":"order-2","customer":"Bob","total":30,"status":"shipped"}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce message");

    // This should not produce any query diff — expect a timeout
    let result = tokio::time::timeout(Duration::from_secs(3), subscription.recv()).await;
    assert!(
        result.is_err(),
        "order-2 (total=30) should not match query, but got a diff"
    );

    // INSERT another matching message
    producer
        .send(
            FutureRecord::to(topic)
                .key("order-3")
                .payload(r#"{"id":"order-3","customer":"Charlie","total":200,"status":"pending"}"#),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce message");

    let result = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
        .await
        .expect("Timed out waiting for result");

    let query_result = result.expect("Expected query result");
    let add = query_result
        .results
        .iter()
        .find(|r| matches!(r, ResultDiff::Add { .. }))
        .expect("Expected an Add result for order-3");
    if let ResultDiff::Add { data, .. } = add {
        assert_eq!(data["id"], "order-3");
    }

    // TOMBSTONE: produce a tombstone for order-1 (null payload = delete)
    producer
        .send(
            FutureRecord::<str, [u8]>::to(topic).key("order-1"),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce tombstone");

    let result = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
        .await
        .expect("Timed out waiting for result");

    let query_result = result.expect("Expected query result");
    let delete = query_result
        .results
        .iter()
        .find(|r| matches!(r, ResultDiff::Delete { .. }))
        .expect("Expected a Delete result for order-1");
    if let ResultDiff::Delete { data, .. } = delete {
        assert_eq!(data["id"], "order-1");
    }

    core.stop().await.unwrap();
}

/// Test Kafka source with custom mapping using operation_from
#[tokio::test]
#[ignore] // Requires Docker
async fn test_kafka_source_custom_mapping() {
    use drasi_source_mapping::{ElementTemplate, ElementType, OperationType, SourceMapping};

    let _ = env_logger::try_init();

    let (_kafka_container, bootstrap_servers) = start_kafka().await;
    let topic = "test-events";
    ensure_topic(&bootstrap_servers, topic, 3).await;

    // Custom mapping that derives operation from payload.action field
    let mappings = vec![SourceMapping {
        operation: None,
        operation_from: Some("payload.action".to_string()),
        operation_map: Some(
            [
                ("created".to_string(), OperationType::Insert),
                ("updated".to_string(), OperationType::Update),
                ("deleted".to_string(), OperationType::Delete),
            ]
            .into_iter()
            .collect(),
        ),
        element_type: ElementType::Node,
        template: ElementTemplate {
            id: "{{payload.id}}".to_string(),
            labels: vec!["Item".to_string()],
            properties: Some(serde_json::json!({
                "id": "{{payload.id}}",
                "name": "{{payload.name}}",
                "price": "{{payload.price}}"
            })),
            from: None,
            to: None,
        },
        when: None,
        effective_from: None,
    }];

    let source = KafkaSource::builder("items_kafka")
        .bootstrap_servers(&bootstrap_servers)
        .topic(topic)
        .group_id("drasi-test-custom")
        .node_label("Item")
        .mappings(mappings)
        .build()
        .unwrap();

    let query = Query::cypher("q1")
        .query("MATCH (i:Item) RETURN i.id AS id, i.name AS name, i.price AS price")
        .from_source("items_kafka")
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("r1")
        .with_query("q1")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();

    let mut subscription = handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(10)),
        )
        .await
        .unwrap();

    // Give source time to connect and assign partitions (5s for slow CI runners)
    sleep(Duration::from_secs(5)).await;

    let producer = create_producer(&bootstrap_servers);

    // INSERT via action field
    producer
        .send(
            FutureRecord::to(topic)
                .key("item-1")
                .payload(r#"{"action":"created","id":"item-1","name":"Widget","price":9.99}"#),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
        .await
        .expect("Timed out waiting for result");

    let query_result = result.expect("Expected query result");
    let add = query_result
        .results
        .iter()
        .find(|r| matches!(r, ResultDiff::Add { .. }))
        .expect("Expected an Add result");
    if let ResultDiff::Add { data, .. } = add {
        assert_eq!(data["name"], "Widget");
    }

    // UPDATE via action field
    producer
        .send(
            FutureRecord::to(topic).key("item-1").payload(
                r#"{"action":"updated","id":"item-1","name":"Super Widget","price":19.99}"#,
            ),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
        .await
        .expect("Timed out waiting for result");

    let query_result = result.expect("Expected query result");
    // Depending on query planner state this may be Update or Add, but the
    // payload must reflect the updated name.
    let updated = query_result.results.iter().find_map(|r| match r {
        ResultDiff::Update { data, .. } | ResultDiff::Add { data, .. } => Some(data),
        _ => None,
    });
    let updated = updated.expect("Expected an updated or added result");
    assert_eq!(updated["name"], "Super Widget");

    // DELETE via action field
    producer
        .send(
            FutureRecord::to(topic).key("item-1").payload(
                r#"{"action":"deleted","id":"item-1","name":"Super Widget","price":19.99}"#,
            ),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
        .await
        .expect("Timed out waiting for result");

    let query_result = result.expect("Expected query result");
    let delete = query_result
        .results
        .iter()
        .find(|r| matches!(r, ResultDiff::Delete { .. }))
        .expect("Expected a Delete result for item-1");
    if let ResultDiff::Delete { data, .. } = delete {
        assert_eq!(data["id"], "item-1");
    }

    core.stop().await.unwrap();
}

async fn produce_order(producer: &FutureProducer, topic: &str, id: &str, total: i64) {
    let payload = format!(r#"{{"id":"{id}","customer":"handover","total":{total}}}"#);
    producer
        .send(
            FutureRecord::to(topic).key(id).payload(&payload),
            Duration::from_secs(5),
        )
        .await
        .expect("Failed to produce order");
}
#[tokio::test]
#[ignore]
async fn test_kafka_bootstrap_source_overlap_handover() {
    let _ = env_logger::try_init();
    let (_kafka_container, bootstrap_servers) = start_kafka().await;
    let topic = format!(
        "test-handover-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    ensure_topic(&bootstrap_servers, &topic, 3).await;
    let producer = create_producer(&bootstrap_servers);
    let initial_count = 50;
    let concurrent_count = 25;
    for i in 0..initial_count {
        produce_order(&producer, &topic, &format!("initial-{i}"), 100).await;
    }
    let source = KafkaSource::builder("orders_handover_kafka")
        .bootstrap_servers(&bootstrap_servers)
        .topic(&topic)
        .group_id("drasi-test-handover")
        .node_label("Order")
        .build()
        .unwrap();
    let bootstrap_provider = KafkaBootstrapProvider::builder()
        .bootstrap_servers(&bootstrap_servers)
        .topic(&topic)
        .node_label("Order")
        .build()
        .unwrap();
    source
        .set_bootstrap_provider(Box::new(bootstrap_provider))
        .await;
    let query = Query::cypher("q1")
        .query("MATCH (o:Order) RETURN o.id AS id")
        .from_source("orders_handover_kafka")
        .build();
    let (reaction, handle) = ApplicationReactionBuilder::new("r1")
        .with_query("q1")
        .build();
    let core = Arc::new(
        DrasiLib::builder()
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );
    let topic_for_concurrent = topic.clone();
    let bootstrap_servers_for_concurrent = bootstrap_servers.clone();
    let concurrent_producer = tokio::spawn(async move {
        let producer = create_producer(&bootstrap_servers_for_concurrent);
        sleep(Duration::from_millis(25)).await;
        for i in 0..concurrent_count {
            produce_order(
                &producer,
                &topic_for_concurrent,
                &format!("concurrent-{i}"),
                100,
            )
            .await;
            sleep(Duration::from_millis(5)).await;
        }
    });
    core.start().await.unwrap();
    let mut subscription = handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(30)),
        )
        .await
        .unwrap();
    concurrent_producer.await.unwrap();
    let expected_ids: Vec<String> = (0..initial_count)
        .map(|i| format!("initial-{i}"))
        .chain((0..concurrent_count).map(|i| format!("concurrent-{i}")))
        .collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while seen.len() < expected_ids.len() && tokio::time::Instant::now() < deadline {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break;
        };
        let result = tokio::time::timeout(remaining, subscription.recv())
            .await
            .expect("Timed out waiting for handover results")
            .expect("Expected query result");
        for diff in result.results {
            let data = match diff {
                ResultDiff::Add { data, .. } | ResultDiff::Update { data, .. } => data,
                _ => continue,
            };
            if let Some(id) = data.get("id").and_then(|v| v.as_str()) {
                if id.starts_with("initial-") || id.starts_with("concurrent-") {
                    *seen.entry(id.to_string()).or_default() += 1;
                }
            }
        }
    }
    for id in &expected_ids {
        assert_eq!(seen.get(id).copied().unwrap_or_default(), 1, "id {id}");
    }
    assert_eq!(seen.len(), expected_ids.len());
    core.stop().await.unwrap();
}
