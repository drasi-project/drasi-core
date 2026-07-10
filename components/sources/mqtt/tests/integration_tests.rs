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
use drasi_lib::channels::ResultDiff;
use drasi_lib::ComponentStatus;
use drasi_lib::{identity::PasswordIdentityProvider, DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_reaction_application::ApplicationReactionHandle;
use drasi_source_mqtt::config::{
    MappingEntity, MappingMode, MappingNode, MappingProperties, MappingRelation,
    MappingRelationEndpoint, MqttTopicConfig, TopicMapping,
};
use drasi_source_mqtt::MqttSource;
use drasi_source_mqtt::{
    config::{MqttSourceConfig, MqttTransportMode},
    connection::MqttConnection,
};
use log::info;
use serial_test::serial;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

const LOCALHOST: &str = "localhost";
const TEST_QUERY_ID: &str = "test-query";
const MQTT_CLIENT_TEST_ID: &str = "mqtt-client-test";
const MQTT_V3_LISTENER: u16 = 1883;
const MQTT_TLS_LISTENER: u16 = 8883;
const SOURCE_RUNNING_TIMEOUT: Duration = Duration::from_secs(20);
const QUERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MESSAGE_PUBLISH_DELAY: Duration = Duration::from_secs(2);

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

fn anonymous_broker(listener: u16) -> MosquittoConfig {
    MosquittoConfig::new()
        .with_listener(listener)
        .with_allow_anonymous(true)
}

fn source_config_for_broker(broker_config: &MosquittoConfig) -> MqttSourceConfig {
    MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        ..Default::default()
    }
}

fn mtls_broker_and_transport(
    protocols: Option<Vec<String>>,
) -> Result<(MosquittoConfig, MqttTransportMode)> {
    let certs = generate_test_certs(LOCALHOST, true).expect("Failed to generate test certificates");

    let client_cert = certs
        .client_cert
        .expect("Client cert should be present for mTLS test");
    let client_key = certs
        .client_key
        .expect("Client key should be present for mTLS test");

    let mut broker_config = MosquittoConfig::new()
        .with_listener(MQTT_TLS_LISTENER)
        .with_allow_anonymous(false)
        .with_require_certificate(true)
        .with_use_identity_as_username(true)
        .with_ca(certs.ca.clone())
        .with_server_cert(certs.server_cert.clone())
        .with_server_key(certs.server_key.clone());

    if let Some(protocols) = protocols {
        broker_config = broker_config.with_protocols(protocols);
    }

    let transport = MqttTransportMode::TLS {
        ca: Some(certs.ca),
        ca_path: None,
        alpn: None,
        client_auth: Some((client_cert.into_bytes(), client_key.into_bytes())),
        client_cert_path: None,
        client_key_path: None,
    };

    Ok((broker_config, transport))
}

fn source_config_with_transport(
    broker_config: &MosquittoConfig,
    transport: MqttTransportMode,
) -> MqttSourceConfig {
    MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        transport: Some(transport),
        ..Default::default()
    }
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
    expected_status: ComponentStatus,
) -> Result<()> {
    let start = Instant::now();

    loop {
        let status = core.get_source_status(source_id).await?;
        if status == expected_status {
            info!("Source {source_id} reached expected status: {status:?}");
            return Ok(());
        }

        if start.elapsed() > SOURCE_RUNNING_TIMEOUT {
            anyhow::bail!(
                "Timed out waiting for source `{source_id}` to reach status {expected_status:?}; current status: {status:?}"
            )
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_source_running(core: &Arc<DrasiLib>, source_id: &str) -> Result<()> {
    wait_for_source_status(core, source_id, ComponentStatus::Running).await
}

async fn wait_for_query_running(core: &Arc<DrasiLib>) -> Result<()> {
    loop {
        let query_status = core.get_query_status(TEST_QUERY_ID).await?;
        if query_status == ComponentStatus::Running {
            return Ok(());
        }
        info!("Waiting for query to be running. Current status: {query_status:?}");
        tokio::time::sleep(QUERY_POLL_INTERVAL).await;
    }
}

async fn publish_v5_messages(
    guard: &mut MosquittoGuard,
    source_config: &MqttSourceConfig,
    messages: &[(&str, &[u8])],
) -> Result<rumqttc::v5::AsyncClient> {
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v5(MQTT_CLIENT_TEST_ID, source_config, None)
            .await
            .expect("Failed to convert config to MQTT options");
    let client = guard
        .get_client_v5(mqtt_options)
        .await
        .expect("Failed to create MQTT client");

    for (topic, payload) in messages {
        client
            .publish(
                *topic,
                rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                false,
                payload.to_vec(),
            )
            .await?;
    }

    Ok(client)
}

async fn publish_v5_messages_after_delay(
    guard: &mut MosquittoGuard,
    source_config: &MqttSourceConfig,
    messages: Vec<(&'static str, &'static [u8])>,
) -> Result<()> {
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v5(MQTT_CLIENT_TEST_ID, source_config, None)
            .await
            .expect("Failed to convert config to MQTT options");
    let client = guard
        .get_client_v5(mqtt_options)
        .await
        .expect("Failed to create MQTT client");

    tokio::spawn(async move {
        tokio::time::sleep(MESSAGE_PUBLISH_DELAY).await;
        for (topic, payload) in messages {
            if let Err(e) = client
                .publish(
                    topic,
                    rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                    false,
                    payload,
                )
                .await
            {
                eprintln!("Failed to publish message: {e:?}");
            }
        }
    })
    .await?;

    Ok(())
}

pub async fn build_core(
    source_config: MqttSourceConfig,
    slot_name: String,
) -> Result<(Arc<DrasiLib>, ApplicationReactionHandle)> {
    let source = MqttSource::builder(slot_name.clone())
        .with_config(source_config)
        .build()
        .await?;

    let query = Query::cypher(TEST_QUERY_ID)
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

    let (reaction, handle) = ApplicationReaction::builder("test-reaction")
        .with_query(TEST_QUERY_ID)
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
    Ok((core, handle))
}

pub async fn build_core_for_input_as_node(
    source_config: MqttSourceConfig,
    slot_name: String,
) -> Result<(Arc<DrasiLib>, ApplicationReactionHandle)> {
    let source = MqttSource::builder(slot_name.clone())
        .with_config(source_config)
        .build()
        .await?;

    let query = Query::cypher(TEST_QUERY_ID)
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

    let (reaction, handle) = ApplicationReaction::builder("test-reaction")
        .with_query(TEST_QUERY_ID)
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mqtt-source-http")
            .with_query(query)
            .with_source(source)
            .build()
            .await?,
    );

    Ok((core, handle))
}

//............................Tests.....

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_publisher_client_connection() -> Result<()> {
    init_logging();

    let broker_config = anonymous_broker(MQTT_V3_LISTENER);
    let source_config = source_config_for_broker(&broker_config);

    // start the mosquitto container.
    let mut guard = MosquittoGuard::new(&broker_config).await?;

    // create the publisher.
    let mqtt_options =
        MqttConnection::config_to_mqtt_options_v3(MQTT_CLIENT_TEST_ID, &source_config, None)
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
async fn test_mqtt_publisher_client_connection_v5() -> Result<()> {
    init_logging();

    let broker_config = anonymous_broker(MQTT_V3_LISTENER);
    let source_config = source_config_for_broker(&broker_config);

    // start the mosquitto container.
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    let client = publish_v5_messages(
        &mut guard,
        &source_config,
        &[("/test/mqtt", b"test message")],
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
    let broker_config = anonymous_broker(MQTT_V3_LISTENER).with_protocols(vec!["4".to_string()]); // only v4 (v3.1.1)
    let source_config = source_config_for_broker(&broker_config);

    // Create the mosquitto container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // Start core with Mqtt Source
    let source_slot_name = slot_name();
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

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
    let broker_config = anonymous_broker(MQTT_V3_LISTENER).with_protocols(vec!["5".to_string()]); // only v5
    let source_config = source_config_for_broker(&broker_config);

    // Create the mosquitto container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // Start core with Mqtt Source
    let source_slot_name = slot_name();
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

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
        host: LOCALHOST.to_string(),
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
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

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
        host: LOCALHOST.to_string(),
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
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

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

    let (broker_config, transport) = mtls_broker_and_transport(None)?;
    let source_config = source_config_with_transport(&broker_config, transport);

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

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

    let (broker_config, transport) = mtls_broker_and_transport(Some(vec!["4".to_string()]))?;
    let source_config = source_config_with_transport(&broker_config, transport);

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

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

    let (broker_config, transport) = mtls_broker_and_transport(Some(vec!["4".to_string()]))?;
    let source_config = MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        max_retries: Some(8),
        base_retry_delay_secs: Some(1),
        transport: Some(transport),
        ..Default::default()
    };

    // start container
    let mut guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to start Mosquitto container");

    // start core with mqtt source
    let source_slot_name = slot_name();
    let (core, _handle) = build_core(source_config, source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

    // stop the contianer to trigger reconnection attempts
    guard.cleanup().await;
    tokio::time::sleep(Duration::from_secs(16)).await; // wait for a few seconds to allow the core to attempt reconnections

    // restart the container
    let new_guard = MosquittoGuard::new(&broker_config)
        .await
        .expect("Failed to restart Mosquitto container");

    // check the status of the source again to ensure it reconnected successfully.
    tokio::time::sleep(Duration::from_secs(16)).await; // wait a bit for the core to reconnect
    wait_for_source_running(&core, &source_slot_name).await?;

    // stop the new container
    new_guard.cleanup().await;
    tokio::time::sleep(Duration::from_secs(1 + 2 + 4 + 8 + 16 + 32 + 64 + 10)).await; // wait for a few seconds to allow the core to attempt reconnections

    wait_for_source_status(&core, &source_slot_name, ComponentStatus::Error).await?;

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

    let (broker_config, transport) = mtls_broker_and_transport(None)?;

    let source_config = MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        transport: Some(transport),
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
                mode: MappingMode::PayloadAsField,
                field_name: Some("reading".to_string()),
                inject_id: Some(true),
                inject: vec![
                    HashMap::from([("floor".to_string(), "{floor}".to_string())]),
                    HashMap::from([("room".to_string(), "{room}".to_string())]),
                ],
            },
            nodes: vec![
                MappingNode {
                    label: "Room".to_string(),
                    id: "{room}".to_string(),
                },
                MappingNode {
                    label: "Floor".to_string(),
                    id: "{floor}".to_string(),
                },
            ],
            relations: vec![
                MappingRelation {
                    label: "LOCATED_IN_ROOM".to_string(),
                    id: "{room}:{device}_located_in_room_{room}".to_string(),
                    from: MappingRelationEndpoint {
                        label: "Device".to_string(),
                        id: "{room}:{device}".to_string(),
                    },
                    to: MappingRelationEndpoint {
                        label: "Room".to_string(),
                        id: "{room}".to_string(),
                    },
                },
                MappingRelation {
                    label: "LOCATED_IN_FLOOR".to_string(),
                    id: "{room}_located_in_floor_{floor}".to_string(),
                    from: MappingRelationEndpoint {
                        label: "Room".to_string(),
                        id: "{room}".to_string(),
                    },
                    to: MappingRelationEndpoint {
                        label: "Floor".to_string(),
                        id: "{floor}".to_string(),
                    },
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
    let (core, _handle) = build_core(source_config.clone(), source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

    // check the status of the query.
    wait_for_query_running(&core).await?;

    publish_v5_messages_after_delay(
        &mut guard,
        &source_config,
        vec![
            ("building/f2/r1/d2", b"25.5"),
            ("building/f1/r2/d2", b"30.2"),
        ],
    )
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
    wait_for_query_results(&core, TEST_QUERY_ID, predicate).await?;

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

    let (broker_config, transport) = mtls_broker_and_transport(None)?;

    let source_config = MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        transport: Some(transport),
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
                    mode: MappingMode::PayloadSpread,
                    field_name: None,
                    inject_id: Some(true),
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
                    mode: MappingMode::PayloadSpread,
                    field_name: None,
                    inject_id: Some(true),
                    inject: vec![
                        HashMap::from([("floor".to_string(), "{floor}".to_string())]),
                        HashMap::from([("room".to_string(), "{room}".to_string())]),
                    ],
                },
                nodes: vec![
                    MappingNode {
                        label: "Room".to_string(),
                        id: "{room}".to_string(),
                    },
                    MappingNode {
                        label: "Floor".to_string(),
                        id: "{floor}".to_string(),
                    },
                ],
                relations: vec![
                    MappingRelation {
                        label: "LOCATED_IN_ROOM".to_string(),
                        id: "{room}:{device}_located_in_room_{room}".to_string(),
                        from: MappingRelationEndpoint {
                            label: "Device".to_string(),
                            id: "{room}:{device}".to_string(),
                        },
                        to: MappingRelationEndpoint {
                            label: "Room".to_string(),
                            id: "{room}".to_string(),
                        },
                    },
                    MappingRelation {
                        label: "LOCATED_IN_FLOOR".to_string(),
                        id: "{room}_located_in_floor_{floor}".to_string(),
                        from: MappingRelationEndpoint {
                            label: "Room".to_string(),
                            id: "{room}".to_string(),
                        },
                        to: MappingRelationEndpoint {
                            label: "Floor".to_string(),
                            id: "{floor}".to_string(),
                        },
                    },
                    MappingRelation {
                        label: "LOCATED_IN_GATEWAY".to_string(),
                        id: "{room}:{device}_located_in_gateway_$.gateway_id".to_string(),
                        from: MappingRelationEndpoint {
                            label: "Device".to_string(),
                            id: "{room}:{device}".to_string(),
                        },
                        to: MappingRelationEndpoint {
                            label: "Gateway".to_string(),
                            id: "$.gateway_id".to_string(),
                        },
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
    let (core, _handle) =
        build_core_for_input_as_node(source_config.clone(), source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

    // check the status of the query.
    wait_for_query_running(&core).await?;

    publish_v5_messages_after_delay(
        &mut guard,
        &source_config,
        vec![
            ("gateway/gateway_1", br#"{"name":"gateway_1"}"#),
            (
                "building/f2/r1/d2",
                br#"{"value":34.0,"gateway_id":"gateway_1"}"#,
            ),
            (
                "building/f2/r1/d1",
                br#"{"value":30.2,"gateway_id":"gateway_1"}"#,
            ),
        ],
    )
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
    wait_for_query_results(&core, TEST_QUERY_ID, predicate).await?;

    info!("Successfully received expected query results");
    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_full_pipeline_with_application_reaction() -> Result<()> {
    init_logging();
    init_rustls_crypto_provider();

    let (broker_config, transport) = mtls_broker_and_transport(None)?;

    let source_config = MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        transport: Some(transport),
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
                mode: MappingMode::PayloadAsField,
                field_name: Some("reading".to_string()),
                inject_id: Some(true),
                inject: vec![
                    HashMap::from([("floor".to_string(), "{floor}".to_string())]),
                    HashMap::from([("room".to_string(), "{room}".to_string())]),
                ],
            },
            nodes: vec![
                MappingNode {
                    label: "Room".to_string(),
                    id: "{room}".to_string(),
                },
                MappingNode {
                    label: "Floor".to_string(),
                    id: "{floor}".to_string(),
                },
            ],
            relations: vec![
                MappingRelation {
                    label: "LOCATED_IN_ROOM".to_string(),
                    id: "{room}:{device}_located_in_room_{room}".to_string(),
                    from: MappingRelationEndpoint {
                        label: "Device".to_string(),
                        id: "{room}:{device}".to_string(),
                    },
                    to: MappingRelationEndpoint {
                        label: "Room".to_string(),
                        id: "{room}".to_string(),
                    },
                },
                MappingRelation {
                    label: "LOCATED_IN_FLOOR".to_string(),
                    id: "{room}_located_in_floor_{floor}".to_string(),
                    from: MappingRelationEndpoint {
                        label: "Room".to_string(),
                        id: "{room}".to_string(),
                    },
                    to: MappingRelationEndpoint {
                        label: "Floor".to_string(),
                        id: "{floor}".to_string(),
                    },
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
    let (core, handle) = build_core(source_config.clone(), source_slot_name.clone()).await?;
    core.start().await?;

    wait_for_source_running(&core, &source_slot_name).await?;

    // create application reaction subscription
    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await?;

    // check the status of the query.
    wait_for_query_running(&core).await?;

    publish_v5_messages_after_delay(
        &mut guard,
        &source_config,
        vec![
            ("building/f2/r1/d2", b"25.5"),
            ("building/f1/r2/d2", b"30.2"),
        ],
    )
    .await?;

    // get the application reaction results
    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    assert_eq!(query_result.query_id, TEST_QUERY_ID);
    assert!(
        query_result.results.len() == 1,
        "Expected exactly one result"
    );

    // verify the results
    match &query_result.results[0] {
        ResultDiff::Add { data, .. } => {
            let value = &data["val"];
            let room = &data["room"];
            let value_ok = value
                .as_f64()
                .map(|v| (v - 25.5).abs() < f64::EPSILON)
                .unwrap_or(false);
            let room_ok = room.as_str().map(|v| v == "r1").unwrap_or(false);
            assert!(
                value_ok && room_ok,
                "Received unexpected query result: {data}"
            );
        }
        _ => panic!("Expected an Add result"),
    }

    // cleanup
    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

// ---------- B8: explicit client_id, two-source no-collision check ----------

/// Two MqttSource instances configured with distinct `client_id`s connect to the same
/// broker without disconnecting each other. Mosquitto v3.1.1/v5 will boot the older
/// session when a second client arrives with the same client_id, so this test would
/// regress if `start()` silently dropped the configured client_id.
#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_explicit_client_id_does_not_collide() -> Result<()> {
    init_logging();

    let broker_config = anonymous_broker(MQTT_V3_LISTENER);

    let mut guard = MosquittoGuard::new(&broker_config).await?;

    let make_config = || MqttSourceConfig {
        host: LOCALHOST.to_string(),
        port: broker_config.listener,
        topics: vec![MqttTopicConfig {
            topic: "client-id-test/+".to_string(),
            qos: drasi_source_mqtt::MqttQoS::ONE,
        }],
        ..Default::default()
    };

    let slot_a = slot_name();
    let slot_b = slot_name();

    let source_a = MqttSource::builder(slot_a.clone())
        .with_config(make_config())
        .with_client_id("drasi-watcher-a")
        .build()
        .await?;

    let source_b = MqttSource::builder(slot_b.clone())
        .with_config(make_config())
        .with_client_id("drasi-watcher-b")
        .build()
        .await?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mqtt-client-id-test")
            .with_source(source_a)
            .with_source(source_b)
            .build()
            .await?,
    );

    core.start().await?;

    wait_for_source_running(&core, &slot_a).await?;
    wait_for_source_running(&core, &slot_b).await?;

    // Hold for a moment so a same-client_id kick would have time to surface as
    // a status transition out of Running on one of the two sources.
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        core.get_source_status(&slot_a).await?,
        ComponentStatus::Running,
        "source A was kicked off the broker"
    );
    assert_eq!(
        core.get_source_status(&slot_b).await?,
        ComponentStatus::Running,
        "source B was kicked off the broker"
    );

    core.stop().await?;
    guard.cleanup().await;
    Ok(())
}

// ---------- S11: orphan topic_mappings pattern fails build() ----------

/// An MQTT subscription that cannot deliver topics matching any configured
/// topic_mappings pattern must fail validation at builder.build() time. Without this
/// check the source would start cleanly, accept messages, and silently drop them all.
#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_orphan_pattern_fails_validation() -> Result<()> {
    init_logging();

    let result = MqttSource::builder("orphan-slot")
        .with_host(LOCALHOST)
        .with_port(MQTT_V3_LISTENER)
        .with_topic_config("sensors/temperature", drasi_source_mqtt::MqttQoS::ONE)
        .with_topic_mappings(vec![TopicMapping {
            pattern: "devices/{id}/telemetry".to_string(),
            entity: MappingEntity {
                label: "Device".to_string(),
                id: "{id}".to_string(),
            },
            properties: MappingProperties {
                mode: MappingMode::PayloadAsField,
                field_name: Some("payload".to_string()),
                inject_id: None,
                inject: vec![],
            },
            nodes: vec![],
            relations: vec![],
        }])
        .build()
        .await;

    let err = match result {
        Ok(_) => panic!("orphan mapping should fail build"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("no compatible subscription"),
        "unexpected error message: {msg}"
    );
    assert!(
        msg.contains("devices/{id}/telemetry"),
        "error did not name the orphan pattern: {msg}"
    );
    Ok(())
}
