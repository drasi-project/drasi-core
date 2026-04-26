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

//! Integration tests for MQTT source using a real Mosquitto broker.
//!
//! These tests validate that the MQTT source correctly receives and processes
//! messages from a real Mosquitto instance via testcontainers.
//!
//! # Running tests
//!
//! ```bash
//! cargo test -p drasi-source-mqtt --test integration_tests -- --ignored --nocapture
//! ```
//!
//! The tests are ignored by default because they require Docker.

mod mosquitto_helpers;
use crate::mosquitto_helpers::{generate_test_certs, MosquittoConfig, MosquittoGuard};
use anyhow::Result;
use drasi_lib::{identity::PasswordIdentityProvider, DrasiLib, Query, Source};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_mqtt::config::{
    InjectId, MappingEntity, MappingProperties, MqttTopicConfig, TopicMapping,
};
use drasi_source_mqtt::MqttSource;
use drasi_source_mqtt::{
    config::{MqttSourceConfig, MqttTransportMode},
    connection::MqttConnection,
    MqttSourceBuilder,
};
use log::info;
use rumqttc::QoS;
use serial_test::serial;
use std::collections::HashMap;
use std::sync::Arc;
use std::{thread::sleep, time::Duration};
use tokio::time::Instant;

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

fn slot_name() -> String {
    format!("drasi_slot_{}", uuid::Uuid::new_v4().simple())
}

async fn wait_for_query_results(
    core: &Arc<DrasiLib>,
    query_id: &str,
    predicate: impl Fn(&[serde_json::Value]) -> bool,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(20);
    loop {
        let results = core.get_query_results(query_id).await?;
        if predicate(&results) {
            info!("Received expected query results: {results:?}");
            return Ok(());
        }
        if start.elapsed() > timeout {
            anyhow::bail!("Timed out waiting for query results for query_id `{query_id}`")
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

pub async fn build_core(
    source_config: MqttSourceConfig,
    slot_name: String,
) -> Result<Arc<DrasiLib>> {
    let source = MqttSource::builder(slot_name.clone())
        .with_config(source_config)
        .build()
        .await?;

    let query = Query::cypher("test-query")
        .query(
            r#"
        MATCH (rd:Device)-[:LOCATED_IN_ROOM]->(r:Room)-[:LOCATED_IN_FLOOR]->(f:Floor)
        WHERE rd.id = 'r1:d2' AND r.id = 'r1' AND f.id = 'f2'
        RETURN rd.reading as val,
               rd.room as room
        "#,
        )
        .from_source(&slot_name)
        .auto_start(true)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder("test-reaction")
        .with_query("test-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mqtt-test-core")
            .with_query(query)
            .with_source(source)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    Ok(core)
}

pub async fn build_core_for_input_as_node(
    source_config: MqttSourceConfig,
    slot_name: String,
) -> Result<Arc<DrasiLib>> {
    let source = MqttSource::builder(slot_name.clone())
        .with_config(source_config)
        .build()
        .await?;

    let query = Query::cypher("test-query")
        .query(
            r#"
        MATCH (d:Device)-[rel:LOCATED_IN_GATEWAY]->(g:Gateway)
        WHERE d.id = 'r1:d2' AND g.id = 'gateway_1'
        RETURN d.value as val,
               g.id as gateway_id
        "#,
        )
        .from_source(&slot_name)
        .auto_start(true)
        .build();

    let (reaction, _handle) = ApplicationReaction::builder("test-reaction")
        .with_query("test-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mqtt-source-http")
            .with_query(query)
            .with_source(source)
            .build()
            .await?,
    );

    Ok(core)
}

//............................Tests.....

#[tokio::test]
#[serial]
#[ignore]
// TODO: This test is currently just testing that we can connect and publish to the broker. We should enhance it to also start the core with an MQTT source and verify that messages are received and processed correctly.
async fn test_mqtt_publisher_client_connection() -> Result<()> {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_listener(1883)
        .with_allow_anonymous(true);

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        ..Default::default()
    };

    // start the mosquitto container.
    let mut guard = MosquittoGuard::new(&broker_config).await?;

    // create the publisher.
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v3("mqtt-client-test", &source_config, None)
            .await
            .expect("Failed to convert config to MQTT options");
    let client = guard
        .get_client(mqtt_options)
        .await
        .expect("Failed to create MQTT client");

    client
        .publish(
            "/test/mqtt",
            rumqttc::QoS::ExactlyOnce,
            false,
            b"test message",
        )
        .await?;

    // cleanup and stop the eventloop before stopping the container
    guard.abort_client_event_loop();
    drop(client);
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
// TODO: This test is currently just testing that we can connect and publish to the broker. We should enhance it to also start the core with an MQTT source and verify that messages are received and processed correctly.
async fn test_mqtt_publisher_client_connection_v5() -> Result<()> {
    init_logging();

    let broker_config = MosquittoConfig::new()
        .with_listener(1883)
        .with_allow_anonymous(true);

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        ..Default::default()
    };

    // start the mosquitto container.
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // create the publisher.
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v5("mqtt-client-test", &source_config, None)
            .await
            .expect("Failed to convert config to MQTT options");
    let client = guard
        .get_client_v5(mqtt_options)
        .await
        .expect("Failed to create MQTT client");

    client
        .publish(
            "/test/mqtt",
            rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
            false,
            "test message".as_bytes(),
        )
        .await?;

    // cleanup and stop the eventloop before stopping the container
    guard.abort_client_event_loop();
    drop(client);
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_version_fallback_from_v5_to_v3() -> Result<()> {
    init_logging();

    // Initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(1883)
        .with_allow_anonymous(true)
        .with_protocols(vec!["4".to_string()]); // only v4 (v3.1.1)

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        ..Default::default()
    };

    // Create the mosquitto container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // Start core with Mqtt Source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_version_v5() -> Result<()> {
    init_logging();

    // Initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(1883)
        .with_allow_anonymous(true)
        .with_protocols(vec!["5".to_string()]); // only v5

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        ..Default::default()
    };

    // Create the mosquitto container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // Start core with Mqtt Source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_static_credentials_authentication() -> Result<()> {
    init_logging();

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_username("testuser")
        .with_password("testpass123");

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        username: Some("testuser".to_string()),
        password: Some("testpass123".to_string()),
        ..Default::default()
    };

    // // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_with_identity_provider_authentication() -> Result<()> {
    init_logging();

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_username("testuser")
        .with_password("testpass123");

    let identity_provider = Box::new(PasswordIdentityProvider::new("testuser", "testpass123"));

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        identity_provider: Some(identity_provider),
        ..Default::default()
    };

    // // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_mtls_connection() -> Result<()> {
    init_logging();
    init_rustls_crypto_provider();

    // Generate certs
    let certs =
        generate_test_certs("localhost", true).expect("Failed to generate test certificates");

    let client_cert = certs
        .client_cert
        .expect("Client cert should be present for mTLS test");
    let client_key = certs
        .client_key
        .expect("Client key should be present for mTLS test");

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_require_certificate(true)
        .with_use_identity_as_username(true)
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        transport: Some(MqttTransportMode::TLS {
            ca: Some(certs.ca.clone()),
            ca_path: None,
            alpn: None,
            client_auth: Some((client_cert.into_bytes(), client_key.into_bytes())),
            client_cert_path: None,
            client_key_path: None,
        }),
        ..Default::default()
    };

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_mtls_connection_v3_1_1() -> Result<()> {
    init_logging();
    init_rustls_crypto_provider();

    // Generate certs
    let certs =
        generate_test_certs("localhost", true).expect("Failed to generate test certificates");

    let client_cert = certs
        .client_cert
        .expect("Client cert should be present for mTLS test");
    let client_key = certs
        .client_key
        .expect("Client key should be present for mTLS test");

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_require_certificate(true)
        .with_use_identity_as_username(true)
        .with_protocols(vec!["4".to_string()])
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        transport: Some(MqttTransportMode::TLS {
            ca: Some(certs.ca.clone()),
            ca_path: None,
            alpn: None,
            client_auth: Some((client_cert.into_bytes(), client_key.into_bytes())),
            client_cert_path: None,
            client_key_path: None,
        }),
        ..Default::default()
    };

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_reconnection_and_loopbackoff() -> Result<()> {
    init_logging();
    init_rustls_crypto_provider();

    // Generate certs
    let certs =
        generate_test_certs("localhost", true).expect("Failed to generate test certificates");

    let client_cert = certs
        .client_cert
        .expect("Client cert should be present for mTLS test");
    let client_key = certs
        .client_key
        .expect("Client key should be present for mTLS test");

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_require_certificate(true)
        .with_use_identity_as_username(true)
        .with_protocols(vec!["4".to_string()])
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        max_retries: Some(8),
        base_retry_delay_secs: Some(1),
        transport: Some(MqttTransportMode::TLS {
            ca: Some(certs.ca.clone()),
            ca_path: None,
            alpn: None,
            client_auth: Some((client_cert.into_bytes(), client_key.into_bytes())),
            client_cert_path: None,
            client_key_path: None,
        }),
        ..Default::default()
    };

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // stop the contianer to trigger reconnection attempts
    guard.cleanup().await;
    tokio::time::sleep(Duration::from_secs(16)).await; // wait for a few seconds to allow the core to attempt reconnections

    // restart the container
    let new_guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to restart Mosquitto container");

    // check the status of the source again to ensure it reconnected successfully.
    tokio::time::sleep(Duration::from_secs(16)).await; // wait a bit for the core to reconnect
    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // stop the new container
    new_guard.cleanup().await;
    tokio::time::sleep(Duration::from_secs(1 + 2 + 4 + 8 + 16 + 32 + 64 + 10)).await; // wait for a few seconds to allow the core to attempt reconnections

    wait_for_source_status(&core, &source_slot_name, drasi_lib::ComponentStatus::Error).await?;

    // cleanup
    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_query_results() -> Result<()> {
    init_logging();
    init_rustls_crypto_provider();

    // Generate certs
    let certs =
        generate_test_certs("localhost", true).expect("Failed to generate test certificates");

    let client_cert = certs
        .client_cert
        .expect("Client cert should be present for mTLS test");
    let client_key = certs
        .client_key
        .expect("Client key should be present for mTLS test");

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_require_certificate(true)
        .with_use_identity_as_username(true)
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        transport: Some(MqttTransportMode::TLS {
            ca: Some(certs.ca.clone()),
            ca_path: None,
            alpn: None,
            client_auth: Some((client_cert.into_bytes(), client_key.into_bytes())),
            client_cert_path: None,
            client_key_path: None,
        }),
        topics: vec![MqttTopicConfig {
            topic: "building/+/+/+".to_string(),
            qos: drasi_source_mqtt::MqttQoS::TWO,
        }],
        topic_mappings: vec![TopicMapping {
            pattern: "building/{floor}/{room}/{device}".to_string(),
            entity: MappingEntity {
                label: "Device".to_string(),
                id: "{room}:{device}".to_string(),
            },
            properties: MappingProperties {
                mode: drasi_source_mqtt::config::MappingMode::PayloadAsField,
                field_name: Some("reading".to_string()),
                inject_id: Some(InjectId::True),
                inject: vec![
                    HashMap::from([("floor".to_string(), "{floor}".to_string())]),
                    HashMap::from([("room".to_string(), "{room}".to_string())]),
                ],
            },
            nodes: vec![
                drasi_source_mqtt::config::MappingNode {
                    label: "Room".to_string(),
                    id: "{room}".to_string(),
                },
                drasi_source_mqtt::config::MappingNode {
                    label: "Floor".to_string(),
                    id: "{floor}".to_string(),
                },
            ],
            relations: vec![
                drasi_source_mqtt::config::MappingRelation {
                    from: "{room}:{device}".to_string(),
                    to: "{room}".to_string(),
                    label: "LOCATED_IN_ROOM".to_string(),
                },
                drasi_source_mqtt::config::MappingRelation {
                    from: "{room}".to_string(),
                    to: "{floor}".to_string(),
                    label: "LOCATED_IN_FLOOR".to_string(),
                },
            ],
        }],
        ..Default::default()
    };

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core = build_core(source_config.clone(), source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // check the status of the query.
    loop {
        let query_status = core.get_query_status("test-query").await?;
        if query_status == drasi_lib::ComponentStatus::Running {
            break;
        }
        info!("Waiting for query to be running. Current status: {query_status:?}",);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // publish messages to the broker
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v5("mqtt-client-test", &source_config, None)
            .await
            .expect("Failed to convert config to MQTT options");
    let client = guard
        .get_client_v5(mqtt_options)
        .await
        .expect("Failed to create MQTT client");

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await; // wait a bit to ensure the core is ready to receive messages
        match client
            .publish(
                "building/f2/r1/d2",
                rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                false,
                "25.5".as_bytes(),
            )
            .await
        {
            Ok(_) => (),
            Err(e) => eprintln!("Failed to publish message: {e:?}"),
        }
        match client
            .publish(
                "building/f1/r2/d2",
                rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                false,
                "30.2".as_bytes(),
            )
            .await
        {
            Ok(_) => (),
            Err(e) => eprintln!("Failed to publish message: {e:?}"),
        }
    })
    .await?;

    // wait for the core to process the messages

    let predicate = |results: &[serde_json::Value]| {
        (results.len() == 1)
            && results.iter().any(|res| {
                let val_ok = res
                    .get("val")
                    .and_then(|v| v.as_f64())
                    .map(|v| (v - 25.5).abs() < f64::EPSILON)
                    .unwrap_or(false);
                let room_ok = res
                    .get("room")
                    .and_then(|v| v.as_str())
                    .map(|v| v == "r1")
                    .unwrap_or(false);
                val_ok && room_ok
            })
    };
    wait_for_query_results(&core, "test-query", predicate).await?;

    info!("Successfully received expected query results");
    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_query_results_with_using_input_data() -> Result<()> {
    init_logging();
    init_rustls_crypto_provider();

    // Generate certs
    let certs =
        generate_test_certs("localhost", true).expect("Failed to generate test certificates");

    let client_cert = certs
        .client_cert
        .expect("Client cert should be present for mTLS test");
    let client_key = certs
        .client_key
        .expect("Client key should be present for mTLS test");

    // initialize config
    let broker_config = MosquittoConfig::new()
        .with_listener(8883)
        .with_allow_anonymous(false)
        .with_require_certificate(true)
        .with_use_identity_as_username(true)
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    let source_config = MqttSourceConfig {
        host: "localhost".to_string(),
        port: broker_config.listener,
        transport: Some(MqttTransportMode::TLS {
            ca: Some(certs.ca.clone()),
            ca_path: None,
            alpn: None,
            client_auth: Some((client_cert.into_bytes(), client_key.into_bytes())),
            client_cert_path: None,
            client_key_path: None,
        }),
        topics: vec![
            MqttTopicConfig {
                topic: "building/+/+/+".to_string(),
                qos: drasi_source_mqtt::MqttQoS::TWO,
            },
            MqttTopicConfig {
                topic: "gateway/+".to_string(),
                qos: drasi_source_mqtt::MqttQoS::TWO,
            },
        ],
        topic_mappings: vec![
            TopicMapping {
                pattern: "gateway/{gateway_id}".to_string(),
                entity: MappingEntity {
                    label: "Gateway".to_string(),
                    id: "{gateway_id}".to_string(),
                },
                properties: MappingProperties {
                    mode: drasi_source_mqtt::config::MappingMode::PayloadSpread,
                    field_name: None,
                    inject_id: Some(InjectId::True),
                    inject: vec![],
                },
                nodes: vec![],
                relations: vec![],
            },
            TopicMapping {
                pattern: "building/{floor}/{room}/{device}".to_string(),
                entity: MappingEntity {
                    label: "Device".to_string(),
                    id: "{room}:{device}".to_string(),
                },
                properties: MappingProperties {
                    mode: drasi_source_mqtt::config::MappingMode::PayloadSpread,
                    field_name: None,
                    inject_id: Some(InjectId::True),
                    inject: vec![
                        HashMap::from([("floor".to_string(), "{floor}".to_string())]),
                        HashMap::from([("room".to_string(), "{room}".to_string())]),
                    ],
                },
                nodes: vec![
                    drasi_source_mqtt::config::MappingNode {
                        label: "Room".to_string(),
                        id: "{room}".to_string(),
                    },
                    drasi_source_mqtt::config::MappingNode {
                        label: "Floor".to_string(),
                        id: "{floor}".to_string(),
                    },
                ],
                relations: vec![
                    drasi_source_mqtt::config::MappingRelation {
                        from: "{room}:{device}".to_string(),
                        to: "{room}".to_string(),
                        label: "LOCATED_IN_ROOM".to_string(),
                    },
                    drasi_source_mqtt::config::MappingRelation {
                        from: "{room}".to_string(),
                        to: "{floor}".to_string(),
                        label: "LOCATED_IN_FLOOR".to_string(),
                    },
                    drasi_source_mqtt::config::MappingRelation {
                        from: "{room}:{device}".to_string(),
                        to: "$.gateway_id".to_string(),
                        label: "LOCATED_IN_GATEWAY".to_string(),
                    },
                ],
            },
        ],
        ..Default::default()
    };

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let core =
        build_core_for_input_as_node(source_config.clone(), source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_status(
        &core,
        &source_slot_name,
        drasi_lib::ComponentStatus::Running,
    )
    .await?;

    // check the status of the query.
    loop {
        let query_status = core.get_query_status("test-query").await?;
        if query_status == drasi_lib::ComponentStatus::Running {
            break;
        }
        info!("Waiting for query to be running. Current status: {query_status:?}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // publish messages to the broker
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v5("mqtt-client-test", &source_config, None)
            .await
            .expect("Failed to convert config to MQTT options");
    let client = guard
        .get_client_v5(mqtt_options)
        .await
        .expect("Failed to create MQTT client");

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await; // wait a bit to ensure the core is ready to receive messages
        match client
            .publish(
                "gateway/gateway_1",
                rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                false,
                r#"{"name":"gateway_1"}"#.as_bytes(),
            )
            .await
        {
            Ok(_) => (),
            Err(e) => eprintln!("Failed to publish message: {e:?}"),
        }
        match client
            .publish(
                "building/f2/r1/d2",
                rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                false,
                r#"{"value":34.0,"gateway_id":"gateway_1"}"#.as_bytes(),
            )
            .await
        {
            Ok(_) => (),
            Err(e) => eprintln!("Failed to publish message: {e:?}"),
        }
        match client
            .publish(
                "building/f2/r1/d1",
                rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                false,
                r#"{"value":30.2,"gateway_id":"gateway_1"}"#.as_bytes(),
            )
            .await
        {
            Ok(_) => (),
            Err(e) => eprintln!("Failed to publish message: {e:?}"),
        }
    })
    .await?;

    // wait for the core to process the messages
    let predicate = |results: &[serde_json::Value]| {
        (results.len() == 1)
            && results.iter().any(|res| {
                let val_ok = res
                    .get("val")
                    .and_then(|v| v.as_f64())
                    .map(|v| (v - 34.0).abs() < f64::EPSILON)
                    .unwrap_or(false);
                let gateway_ok = res
                    .get("gateway_id")
                    .and_then(|v| v.as_str())
                    .map(|v| v == "gateway_1")
                    .unwrap_or(false);
                val_ok && gateway_ok
            })
    };
    wait_for_query_results(&core, "test-query", predicate).await?;

    info!("Successfully received expected query results");
    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}
