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

//! Integration tests for MQTT reaction using a real Mosquitto broker.
//!
//! These tests validate that the MQTT reaction can connect to a broker, and
//! publish messages to a real Mosquitto broker using test containers.
//!
//! # Running tests
//!
//! ```bash
//! cargo test -p drasi-reaction-mqtt --test integration_tests -- --ignored --nocapture
//! ```
//!
//! The tests are ignored by default because they require Docker.

mod mosquitto_helpers;
use crate::mosquitto_helpers::generate_test_certs;
use anyhow::Result;
use drasi_lib::identity::PasswordIdentityProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_mqtt::config::{
    MqttExtension, MqttProtocolVersion, MqttQoS, MqttReactionConfig, QueryConfig, TemplateSpec,
};
use drasi_reaction_mqtt::MqttReactionBuilder;
use drasi_source_http::HttpSource;
use log::info;
use rumqttc::MqttOptions;
use serial_test::serial;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use crate::mosquitto_helpers::{MosquittoConfig, MosquittoGuard};

fn init_rustls_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();

    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

fn mqtt_extension(topic: &str, qos: MqttQoS) -> MqttExtension {
    MqttExtension {
        topic: topic.to_string(),
        qos,
        retain: false,
        empty_payload: false,
        message_expiry_interval: None,
    }
}

fn query_config(qos: MqttQoS) -> QueryConfig<MqttExtension> {
    QueryConfig {
        added: Some(TemplateSpec::with_extension(
            "[{{query_name}}] + {{after.symbol}}: ${{after.price}}",
            mqtt_extension("stocks/{{query_name}}/added", qos),
        )),
        updated: Some(TemplateSpec::with_extension(
            "[{{query_name}}] ~ {{after.symbol}}: ${{before.price}} -> ${{after.price}}",
            mqtt_extension("stocks/{{query_name}}/updated", qos),
        )),
        deleted: Some(TemplateSpec::with_extension(
            "[{{query_name}}] - {{before.symbol}} removed",
            mqtt_extension("stocks/{{query_name}}/deleted", qos),
        )),
    }
}

fn slot_name() -> String {
    format!("drasi_slot_{}", uuid::Uuid::new_v4().simple())
}

pub async fn build_core(
    reaction_config: MqttReactionConfig,
    slot_name: String,
) -> Result<Arc<DrasiLib>> {
    // create the source for data injection
    let http_source = HttpSource::builder("stocks-test-source")
        .with_host("0.0.0.0")
        .with_port(9000)
        .build()?;

    // create the query
    let query = Query::cypher("stocks_query")
        .query(
            r#"
            MATCH (p:stock_prices)
            RETURN p.symbol AS symbol,
                   p.price AS price
        "#,
        )
        .from_source("stocks-test-source")
        .auto_start(true)
        .build();

    // create the reaction
    let reaction = MqttReactionBuilder::new(slot_name)
        .with_config(reaction_config)
        .with_query("stocks_query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mqtt-test-core")
            .with_query(query)
            .with_source(http_source)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    Ok(core)
}

async fn wait_for_reaction_status(
    core: &Arc<DrasiLib>,
    reaction_id: &str,
    expected_status: drasi_lib::ComponentStatus,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        let status = core.get_reaction_status(reaction_id).await?;
        if status == expected_status {
            info!("Reaction {reaction_id} reached expected status: {status:?}");
            return Ok(());
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for reaction `{reaction_id}` to reach status {expected_status:?}; current status: {status:?}"
            )
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_source_status(
    core: &Arc<DrasiLib>,
    source_id: &str,
    expected_status: drasi_lib::ComponentStatus,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        let status = core.get_source_status(source_id).await?;
        if status == expected_status {
            info!("Source {source_id} reached expected status: {status:?}");
            return Ok(());
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for source `{source_id}` to reach status {expected_status:?}; current status: {status:?}"
            )
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

//.....................................Tests....

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// Test that the MQTT reaction can connect to a real Mosquitto broker using Username/Password authentication and publish messages.
async fn test_mqtt_reaction_with_authentication() {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_username("testuser")
        .with_password("testpassword")
        .with_allow_anonymous(false)
        .with_protocols(vec!["5".to_string()])
        .with_listener(1883);

    let reaction_config = MqttReactionConfig {
        url: "mqtt://localhost:1883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(query_config(MqttQoS::AtLeastOnce)),
        identity_provider: Some(Box::new(PasswordIdentityProvider::new(
            "testuser".to_string(),
            "testpassword".to_string(),
        ))),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut _guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status");
}

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// Verifies authentication failure is handled correctly when connecting to a Mosquitto broker with wrong credentials.
async fn test_mqtt_reaction_authentication_failure() {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_username("testuser")
        .with_password("wrongpassword")
        .with_allow_anonymous(false)
        .with_protocols(vec!["5".to_string()])
        .with_listener(1883);

    let reaction_config = MqttReactionConfig {
        url: "mqtt://localhost:1883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(query_config(MqttQoS::AtLeastOnce)),
        identity_provider: Some(Box::new(PasswordIdentityProvider::new(
            "testuser".to_string(),
            "testpasword".to_string(),
        ))),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut _guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Error)
        .await
        .expect("Reaction did not reach Error status");
}

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// Test that the MQTT reaction publish messages to a real Mosquitto broker at QoS 0
async fn test_mqtt_reaction_publish_qos0() {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_username("testuser")
        .with_password("testpassword")
        .with_allow_anonymous(false)
        .with_protocols(vec!["5".to_string(), "4".to_string()])
        .with_listener(1883);

    let reaction_config = MqttReactionConfig {
        url: "mqtt://localhost:1883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(query_config(MqttQoS::AtMostOnce)),
        identity_provider: Some(Box::new(PasswordIdentityProvider::new(
            "testuser".to_string(),
            "testpassword".to_string(),
        ))),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status");

    // wait for the source to be running
    wait_for_source_status(
        &core,
        "stocks-test-source",
        drasi_lib::ComponentStatus::Running,
    )
    .await
    .expect("Source did not reach Running status");

    // create a subscriber to verify messages are received by the broker
    let mut options = MqttOptions::new("testclient", "localhost", 1883);
    options.set_credentials("testuser", "testpassword");
    let (client, mut event_loop) = guard
        .get_subscriber(options)
        .await
        .expect("Failed to create MQTT subscriber");

    client
        .subscribe("stocks/#", rumqttc::QoS::AtMostOnce)
        .await
        .expect("Failed to subscribe to topic");

    loop {
        match event_loop.poll().await.expect("Failed to poll event loop") {
            rumqttc::Event::Incoming(rumqttc::Packet::SubAck(_)) => break,
            _ => continue,
        }
    }

    // inject data to the http source to trigger the reaction
    let http_client = reqwest::Client::new();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await; // wait a bit to ensure the subscriber is ready
        let payload = r#"
            {
                "operation": "insert",
                "element": {
                    "type": "node",
                    "id": "price_AAPL",
                    "labels": ["stock_prices"],
                    "properties": {
                        "symbol": "AAPL",
                        "price": 179.10
                    }
                }
            }
            "#;
        let response = http_client
            .post("http://localhost:9000/sources/stocks-test-source/events")
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .expect("Failed to send data to HTTP source");
        assert!(
            response.status().is_success(),
            "HTTP source rejected event with status {}",
            response.status()
        );
    });

    // start the event loop to receive messages from the broker and verify the reaction published the expected message
    let mut received_message = false;
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        if start.elapsed() > timeout {
            panic!("Timed out waiting for message from broker");
        }

        let event = event_loop.poll().await.expect("Failed to poll event loop");
        if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = event {
            let payload_str = String::from_utf8_lossy(&publish.payload);
            info!(
                "Received message on topic {}: {}",
                publish.topic, payload_str
            );
            if publish.topic == "stocks/stocks_query/added"
                && payload_str.eq("[stocks_query] + AAPL: $179.1")
            {
                received_message = true;
                break;
            }
        }
    }

    assert!(
        received_message,
        "Did not receive expected message from broker"
    );
}

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// Test that the MQTT reaction publish messages to a real Mosquitto broker at QoS 1
async fn test_mqtt_reaction_publish_qos1() {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_username("testuser")
        .with_password("testpassword")
        .with_allow_anonymous(false)
        .with_protocols(vec!["5".to_string(), "4".to_string()])
        .with_listener(1883);

    let reaction_config = MqttReactionConfig {
        url: "mqtt://localhost:1883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(query_config(MqttQoS::AtLeastOnce)),
        identity_provider: Some(Box::new(PasswordIdentityProvider::new(
            "testuser".to_string(),
            "testpassword".to_string(),
        ))),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status");

    // wait for the source to be running
    wait_for_source_status(
        &core,
        "stocks-test-source",
        drasi_lib::ComponentStatus::Running,
    )
    .await
    .expect("Source did not reach Running status");

    // create a subscriber to verify messages are received by the broker
    let mut options = MqttOptions::new("testclient", "localhost", 1883);
    options.set_credentials("testuser", "testpassword");
    let (client, mut event_loop) = guard
        .get_subscriber(options)
        .await
        .expect("Failed to create MQTT subscriber");

    client
        .subscribe("stocks/#", rumqttc::QoS::AtMostOnce)
        .await
        .expect("Failed to subscribe to topic");

    loop {
        match event_loop.poll().await.expect("Failed to poll event loop") {
            rumqttc::Event::Incoming(rumqttc::Packet::SubAck(_)) => break,
            _ => continue,
        }
    }

    // inject data to the http source to trigger the reaction
    let http_client = reqwest::Client::new();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await; // wait a bit to ensure the subscriber is ready
        let payload = r#"
            {
                "operation": "insert",
                "element": {
                    "type": "node",
                    "id": "price_AAPL",
                    "labels": ["stock_prices"],
                    "properties": {
                        "symbol": "AAPL",
                        "price": 179.10
                    }
                }
            }
            "#;
        let response = http_client
            .post("http://localhost:9000/sources/stocks-test-source/events")
            .header("Content-Type", "application/json")
            .body(payload)
            .send()
            .await
            .expect("Failed to send data to HTTP source");
        assert!(
            response.status().is_success(),
            "HTTP source rejected event with status {}",
            response.status()
        );
    });

    // start the event loop to receive messages from the broker and verify the reaction published the expected message
    let mut received_message = false;
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        if start.elapsed() > timeout {
            panic!("Timed out waiting for message from broker");
        }

        let event = event_loop.poll().await.expect("Failed to poll event loop");
        if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = event {
            let payload_str = String::from_utf8_lossy(&publish.payload);
            info!(
                "Received message on topic {}: {}",
                publish.topic, payload_str
            );
            if publish.topic == "stocks/stocks_query/added"
                && payload_str.eq("[stocks_query] + AAPL: $179.1")
            {
                received_message = true;
                break;
            }
        }
    }

    assert!(
        received_message,
        "Did not receive expected message from broker"
    );
}

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// Test the publish with retained messages and empty payloads to a real Mosquitto broker.
async fn test_mqtt_reaction_publish_retained_and_empty_payload() {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_username("testuser")
        .with_password("testpassword")
        .with_allow_anonymous(false)
        .with_protocols(vec!["5".to_string(), "4".to_string()])
        .with_listener(1883);

    let mut default_query_config = query_config(MqttQoS::AtMostOnce);
    if let Some(delete_template) = &mut default_query_config.deleted {
        delete_template.extension.retain = true;
        delete_template.extension.empty_payload = true;
    }

    let reaction_config = MqttReactionConfig {
        url: "mqtt://localhost:1883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(default_query_config),
        identity_provider: Some(Box::new(PasswordIdentityProvider::new(
            "testuser".to_string(),
            "testpassword".to_string(),
        ))),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status");

    // wait for the source to be running
    wait_for_source_status(
        &core,
        "stocks-test-source",
        drasi_lib::ComponentStatus::Running,
    )
    .await
    .expect("Source did not reach Running status");

    // inject data to the http source to trigger the reaction
    let http_client = reqwest::Client::new();
    let payload = r#"
        {
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "price_AAPL",
                "labels": ["stock_prices"],
                "properties": {
                    "symbol": "AAPL",
                    "price": 179.10
                }
            }
        }
        "#;
    let response = http_client
        .post("http://localhost:9000/sources/stocks-test-source/events")
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .expect("Failed to send data to HTTP source");
    assert!(
        response.status().is_success(),
        "HTTP source rejected event with status {}",
        response.status()
    );

    // delete the node to trigger the delete template
    let delete_payload = r#"
    {
        "operation": "delete",
        "id": "price_AAPL",
        "labels": ["stock_prices"]
    }
    "#;
    let delete_response = http_client
        .post("http://localhost:9000/sources/stocks-test-source/events")
        .header("Content-Type", "application/json")
        .body(delete_payload)
        .send()
        .await
        .expect("Failed to send delete event to HTTP source");
    assert!(
        delete_response.status().is_success(),
        "HTTP source rejected delete event with status {}",
        delete_response.status()
    );

    // create a subscriber to verify messages are received by the broker
    let mut options = MqttOptions::new("testclient", "localhost", 1883);
    options.set_credentials("testuser", "testpassword");
    let (client, mut event_loop) = guard
        .get_subscriber(options)
        .await
        .expect("Failed to create MQTT subscriber");

    client
        .subscribe("stocks/#", rumqttc::QoS::AtMostOnce)
        .await
        .expect("Failed to subscribe to topic");

    // start the event loop to receive messages from the broker and verify the reaction published the expected message
    let mut received_message = false;
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        if start.elapsed() > timeout {
            panic!("Timed out waiting for message from broker");
        }

        let event = event_loop.poll().await.expect("Failed to poll event loop");
        if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = event {
            let payload_str = String::from_utf8_lossy(&publish.payload);
            info!(
                "Received message on topic {}: {}",
                publish.topic, payload_str
            );
            if publish.topic == "stocks/stocks_query/deleted" && payload_str.is_empty() {
                received_message = true;
                break;
            }
        }
    }

    assert!(
        received_message,
        "Did not receive expected message from broker"
    );
}

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// test reconnection after connection loss to a real Mosquitto broker (broker restart).
async fn test_mqtt_reaction_reconnection() {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_username("testuser")
        .with_password("testpassword")
        .with_allow_anonymous(false)
        .with_protocols(vec!["5".to_string(), "4".to_string()])
        .with_listener(1883);

    let reaction_config = MqttReactionConfig {
        url: "mqtt://localhost:1883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(query_config(MqttQoS::AtLeastOnce)),
        identity_provider: Some(Box::new(PasswordIdentityProvider::new(
            "testuser".to_string(),
            "testpassword".to_string(),
        ))),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status");

    // wait for the source to be running
    wait_for_source_status(
        &core,
        "stocks-test-source",
        drasi_lib::ComponentStatus::Running,
    )
    .await
    .expect("Source did not reach Running status");

    // restart the broker to trigger reconnection
    guard.cleanup().await;

    // sleep a bit
    tokio::time::sleep(Duration::from_secs(5)).await;

    // verify the reaction status is Running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status after broker restart");

    // start the broker again to verify the reaction can reconnect to the restarted broker
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create a subscriber to verify messages are received by the broker.
    let mut options = MqttOptions::new("testclient", "localhost", 1883);
    options.set_credentials("testuser", "testpassword");
    let (client, mut event_loop) = guard
        .get_subscriber(options)
        .await
        .expect("Failed to create MQTT subscriber");

    client
        .subscribe("stocks/#", rumqttc::QoS::AtMostOnce)
        .await
        .expect("Failed to subscribe to topic");

    // send message to the HTTP source to trigger the reaction and verify it can publish to the restarted broker.
    let http_client = reqwest::Client::new();
    let payload = r#"
    {
        "operation": "insert",
        "element": {
            "type": "node",
            "id": "price_AAPL",
            "labels": ["stock_prices"],
            "properties": {
                "symbol": "AAPL",
                "price": 179.10
            }
        }
    }
    "#;
    let response = http_client
        .post("http://localhost:9000/sources/stocks-test-source/events")
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .expect("Failed to send data to HTTP source");
    assert!(
        response.status().is_success(),
        "HTTP source rejected event with status {}",
        response.status()
    );

    // wait for a message to be received
    let mut received_message = false;
    while let Ok(event) = event_loop.poll().await {
        if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = event {
            let payload_str = String::from_utf8_lossy(&publish.payload);
            info!(
                "Received message on topic {}: {}",
                publish.topic, payload_str
            );
            if publish.topic == "stocks/stocks_query/added"
                && payload_str.eq("[stocks_query] + AAPL: $179.1")
            {
                received_message = true;
                break;
            }
        }
    }

    assert!(
        received_message,
        "Did not receive expected message from broker"
    );
}

#[tokio::test]
#[serial]
#[ignore = "Requires Docker and a running Mosquitto broker"]
/// Test that the TLS handlshake works correctly when connecting to a Mosquitto broker.
async fn test_mqtt_reaction_tls_handshake() {
    init_rustls_crypto_provider();

    // Generate certs
    let certs =
        generate_test_certs("localhost", false).expect("Failed to generate test certificates");

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(true)
        .with_protocols(vec!["4".to_string(), "5".to_string()])
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    // create the reaction config with TLS
    let reaction_config = MqttReactionConfig {
        url: "mqtts://localhost:8883".to_string(),
        protocol_version: MqttProtocolVersion::V5,
        default_template: Some(query_config(MqttQoS::AtLeastOnce)),
        tls: Some(drasi_reaction_mqtt::config::MqttTlsConfig {
            ca: Some(certs.ca.clone().into_bytes()),
            ..Default::default()
        }),
        ..Default::default()
    };

    // start the mosquitto broker
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto broker");

    // create the core and reaction
    let slot_name = slot_name();
    let core = build_core(reaction_config, slot_name.clone())
        .await
        .expect("Failed to build core and reaction");

    core.start().await.expect("Failed to start core");

    // wait for the reaction to be running
    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running)
        .await
        .expect("Reaction did not reach Running status");

    // wait for the source to be running
    wait_for_source_status(
        &core,
        "stocks-test-source",
        drasi_lib::ComponentStatus::Running,
    )
    .await
    .expect("Source did not reach Running status");

    // create a subscriber to verify messages are received by the broker
    let mut options = MqttOptions::new("testclient", "localhost", 8883);
    options.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Simple {
        ca: certs.ca.clone().into_bytes(),
        alpn: None,
        client_auth: None,
    }));
    let (client, mut event_loop) = guard
        .get_subscriber(options)
        .await
        .expect("Failed to create MQTT subscriber");

    client
        .subscribe("stocks/#", rumqttc::QoS::AtMostOnce)
        .await
        .expect("Failed to subscribe to topic");

    loop {
        match event_loop.poll().await.expect("Failed to poll event loop") {
            rumqttc::Event::Incoming(rumqttc::Packet::SubAck(_)) => break,
            _ => continue,
        }
    }

    // inject data to the http source to trigger the reaction
    let http_client = reqwest::Client::new();
    let payload = r#"
    {
        "operation": "insert",
        "element": {
            "type": "node",
            "id": "price_AAPL",
            "labels": ["stock_prices"],
            "properties": {
                "symbol": "AAPL",
                "price": 179.10
            }
        }
    }
    "#;
    let response = http_client
        .post("http://localhost:9000/sources/stocks-test-source/events")
        .header("Content-Type", "application/json")
        .body(payload)
        .send()
        .await
        .expect("Failed to send data to HTTP source");
    assert!(
        response.status().is_success(),
        "HTTP source rejected event with status {}",
        response.status()
    );

    // start the event loop to receive messages from the broker and verify the reaction published the expected message
    let mut received_message = false;
    let start = Instant::now();
    let timeout = Duration::from_secs(20);

    loop {
        if start.elapsed() > timeout {
            panic!("Timed out waiting for message from broker");
        }

        let event = event_loop.poll().await.expect("Failed to poll event loop");
        if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = event {
            let payload_str = String::from_utf8_lossy(&publish.payload);
            info!(
                "Received message on topic {}: {}",
                publish.topic, payload_str
            );
            if publish.topic == "stocks/stocks_query/added"
                && payload_str.eq("[stocks_query] + AAPL: $179.1")
            {
                received_message = true;
                break;
            }
        }
    }

    assert!(
        received_message,
        "Did not receive expected message from broker"
    );
}

// Still edge cases need to be added.
