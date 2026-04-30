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

use core::error;
use std::time::Duration;

use crate::config::{
    self, default_base_retry_delay_secs, default_max_retries, MqttQoS, MqttSourceConfig,
    MqttTransportMode,
};
use crate::utils::MqttPacket;
use log::{error, info};
use rumqttc::v5::{
    AsyncClient as AsyncClientV5, EventLoop as EventLoopV5, MqttOptions as MqttOptionsV5,
};
use rumqttc::{
    v5::{mqttbytes::v5::ConnectReturnCode, ConnectionError},
    AsyncClient, EventLoop, MqttOptions,
};
use std::sync::Arc;

macro_rules! common_security_config_to_mqtt_options {
    ($options:expr, $config:expr, $identity_provider:expr) => {
        let mut optional_mtls_client_auth = None;
        if let Some(provider) = $identity_provider.as_ref() {
            let context = drasi_lib::identity::CredentialContext::new()
                .with_property("hostname", &$config.host)
                .with_property("port", $config.port.to_string());
            let credentials = provider.get_credentials(&context).await;
            match credentials {
                Ok(creds) => {
                    info!("Successfully retrieved credentials from identity provider for MQTT authentication");
                    match creds {
                        drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
                            $options.set_credentials(username, password);
                        }
                        drasi_lib::identity::Credentials::Token { username, token } => {
                            $options.set_credentials(username, token);
                        }
                        drasi_lib::identity::Credentials::Certificate { cert_pem, key_pem, .. } => { // not currently implemented in the identity provider.
                            optional_mtls_client_auth = Some((cert_pem.clone(), key_pem.clone()));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to retrieve credentials from identity provider for MQTT authentication: {e:?}");
                    return Err(anyhow::anyhow!("Failed to retrieve credentials from identity provider for MQTT authentication: {e:?}"));
                }
            }
        } else {
            info!("No identity provider configured for MQTT authentication, trying static credentials from config if provided");
            if let (Some(username), Some(password)) = ($config.username.as_ref(), $config.password.as_ref()) {
                info!("Using static credentials from config for MQTT authentication");
                $options.set_credentials(username.clone(), password.clone());
            } else {
                info!("No static credentials provided in config for MQTT authentication, attempt to connect without authentication if the broker allows anonymous connections, otherwise connection will fail and be logged accordingly");
            }
        }

        // TLS configuration - mTLS client auth from identity provider takes precedence over static config if both are provided
        if let Some(transport) = $config.transport.as_ref() {
            match transport {
                MqttTransportMode::TLS{..} => {
                    let resolved = transport
                        .get_tls_config()?;

                    // mTLS client auth from identity provider takes precedence over static config if both are provided
                    if let Some((cert_pem, key_pem)) = optional_mtls_client_auth {
                        let tls_config = rumqttc::TlsConfiguration::Simple {
                            ca: resolved.ca,
                            alpn: resolved.alpn,
                            client_auth: Some((cert_pem.into_bytes(), key_pem.into_bytes())),
                        };
                        $options.set_transport(rumqttc::Transport::Tls(tls_config));

                    } else {
                        let tls_config = rumqttc::TlsConfiguration::Simple {
                            ca: resolved.ca,
                            alpn: resolved.alpn,
                            client_auth: resolved.client_auth,
                        };
                        $options.set_transport(rumqttc::Transport::Tls(tls_config));
                    }
                },
                MqttTransportMode::TCP => { // this is the default transport in rumqttc.
                    $options.set_transport(rumqttc::Transport::Tcp);
                }
            }
        }

    };
}

macro_rules! common_config_to_mqtt_options {
    ($options:expr, $config:expr, $identity_provider:expr) => {
        if let Some(request_channel_capacity) = $config.request_channel_capacity {
            $options.set_request_channel_capacity(request_channel_capacity);
        }

        if let Some(keep_alive) = $config.keep_alive {
            $options.set_keep_alive(Duration::from_secs(keep_alive));
        }

        common_security_config_to_mqtt_options!($options, $config, $identity_provider);
    };
}

trait ToMqttPacket {
    fn to_mqtt_packet(self) -> Option<MqttPacket>;
}

impl ToMqttPacket for rumqttc::Event {
    fn to_mqtt_packet(self) -> Option<MqttPacket> {
        if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) = self {
            Some(MqttPacket {
                topic: p.topic,
                payload: p.payload,
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
            })
        } else {
            None
        }
    }
}

impl ToMqttPacket for rumqttc::v5::Event {
    fn to_mqtt_packet(self) -> Option<MqttPacket> {
        if let rumqttc::v5::Event::Incoming(rumqttc::v5::mqttbytes::v5::Packet::Publish(p)) = self {
            Some(MqttPacket {
                topic: str::from_utf8(&p.topic).unwrap_or_default().to_string(),
                payload: p.payload,
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
            })
        } else {
            None
        }
    }
}

pub enum MqttAsyncClientWrapper {
    AsyncClientV3(AsyncClient),
    AsyncClientV5(AsyncClientV5),
}

pub enum MqttEventLoopWrapper {
    EventLoopV3 { event_loop: EventLoop },
    EventLoopV5 { event_loop: EventLoopV5 },
}

/// Mqtt connection manager that handles connection setup, authentication, and event loop management for both MQTT v5 and v3.1.1 brokers based on configuration and broker capabilities.
pub struct MqttConnection {
    client: MqttAsyncClientWrapper,
    identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    config: Option<MqttSourceConfig>,
}

impl MqttConnection {
    //....... Public methods

    pub async fn new(
        id: impl Into<String>,
        config: &MqttSourceConfig,
        identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> anyhow::Result<(Self, MqttEventLoopWrapper)> {
        let id_v5 = id.into().clone();
        let id_v3 = id_v5.clone();

        // authentication with the identity provider (higher precedence than static credentials from config if both are provided)
        let effective_provider = identity_provider;
        let config_provider = config
            .identity_provider
            .as_ref()
            .map(|p| Arc::from(p.clone_box()));

        let provider = effective_provider.or(config_provider);

        // try Mqtt v5 first
        match Self::mqtt5_connect(id_v5, config, &provider).await {
            Ok((client, event_loop)) => {
                let mut connection = Self {
                    client: MqttAsyncClientWrapper::AsyncClientV5(client),
                    identity_provider: provider,
                    config: Some(config.clone()),
                };
                return Ok((connection, MqttEventLoopWrapper::EventLoopV5 { event_loop }));
            }
            Err(e) => {
                error!("MQTT v5 connection failed: {e:?}. Attempting MQTT v3 fallback...");
            }
        }

        // fallback to Mqtt v3.1.1
        match Self::mqttv3_connect(id_v3, config, &provider).await {
            Ok((client_v3, event_loop_v3)) => {
                let mut connection = Self {
                    client: MqttAsyncClientWrapper::AsyncClientV3(client_v3),
                    identity_provider: provider,
                    config: Some(config.clone()),
                };
                return Ok((
                    connection,
                    MqttEventLoopWrapper::EventLoopV3 {
                        event_loop: event_loop_v3,
                    },
                ));
            }
            Err(e) => {
                error!("MQTT v3 connection failed: {e:?}.");
            }
        }

        Err(anyhow::anyhow!("Failed to create MQTT client with both v5 and v3 options. Check configuration for errors."))
    }

    pub async fn run_subscription_loop(
        &mut self,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        mut event_loop: MqttEventLoopWrapper,
        config: &MqttSourceConfig,
        mut processer_tx: tokio::sync::mpsc::Sender<MqttPacket>,
    ) -> anyhow::Result<()> {
        self.subscribe_to_topics(config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to topics: {e:?}"))?;

        let mqtt_version: u8 = match &self.client {
            MqttAsyncClientWrapper::AsyncClientV5(_) => 5,
            MqttAsyncClientWrapper::AsyncClientV3(_) => 3,
        };

        let max_retries = config
            .max_retries
            .unwrap_or(default_max_retries().unwrap_or(8));
        let base_retry_delay_secs = config
            .base_retry_delay_secs
            .unwrap_or(default_base_retry_delay_secs().unwrap_or(1));
        let mut error_count = 0;
        let mut doubler = 1;

        if mqtt_version == 5 {
            let mut event_loop_v5 = match event_loop {
                MqttEventLoopWrapper::EventLoopV5 { event_loop } => event_loop,
                _ => {
                    error!("Expected MQTT v5 event loop, exiting..."); // unexpected error since we should only get here if MQTT v5 connection was successful.
                    return Err(anyhow::anyhow!("Expected MQTT v5 event loop, exiting..."));
                }
            };
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("Shutdown signal received, stopping MQTT event loop");
                        break;
                    },
                    event = event_loop_v5.poll() => {
                        match event {
                            Ok(rumqttc::v5::Event::Incoming(rumqttc::v5::mqttbytes::v5::Packet::ConnAck(_))) => {
                                info!("Successfully connected to MQTT broker using MQTT v5 options");
                                self.subscribe_to_topics(config).await.unwrap_or_else(|e| {
                                    error!("Failed to subscribe to topics: {e:?}");
                                });
                                error_count = 0;
                                doubler = 1;
                            },
                            Ok(event) => {
                                let packet = event.to_mqtt_packet();
                                if let Some(packet) = packet {
                                    if let Err(e) = processer_tx.send(packet).await {
                                            error!("Failed to send MQTT packet to processor: {e:?}");
                                    }
                                }
                                error_count = 0;
                                doubler = 1;
                            },
                            Err(e) => {
                                error_count += 1;
                                if error_count >= max_retries {
                                    error!("MQTT event loop error: {e:?}. Encountered {error_count} consecutive errors, exceeding the maximum of {max_retries}. Stopping the event loop.");
                                    return Err(anyhow::anyhow!("MQTT event loop has encountered {error_count} consecutive errors, exceeding the maximum of {max_retries}. Stopping the event loop."));
                                } else {
                                    let delay = base_retry_delay_secs * doubler;
                                    info!("MQTT event loop error: {e:?}. Waiting for {delay} seconds before retrying MQTT event loop...");
                                    tokio::time::sleep(Duration::from_secs(delay)).await;

                                    // refresh MQTT options security settings.
                                    self.set_mqtt_options_v5_security(&mut event_loop_v5.options).await.unwrap_or_else(|e| {
                                        error!("Failed to update MQTT options with security credentials: {e:?}");
                                    });
                                    doubler *= 2; // exponential backoff
                                }
                            }
                        }
                    }
                }
            }
        } else {
            let mut event_loop_v3 = match event_loop {
                MqttEventLoopWrapper::EventLoopV3 { event_loop } => event_loop,
                _ => {
                    error!("Expected MQTT v3 event loop, exiting..."); // unexpected error since we should only get here if MQTT v3 connection was successful.
                    return Err(anyhow::anyhow!("Expected MQTT v3 event loop, exiting..."));
                }
            };

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("Shutdown signal received, stopping MQTT event loop");
                        break;
                    },
                    event = event_loop_v3.poll() => {
                        match event {
                            Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                                info!("Successfully connected to MQTT broker using MQTT v3 options");
                                self.subscribe_to_topics(config).await.unwrap_or_else(|e| {
                                    error!("Failed to subscribe to topics: {e:?}");
                                });
                                error_count = 0;
                                doubler = 1;
                            },
                            Ok(event) => {
                                let packet = event.to_mqtt_packet();
                                if let Some(packet) = packet {
                                    if let Err(e) = processer_tx.send(packet).await {
                                            error!("Failed to send MQTT packet to processor: {e:?}");
                                    }
                                }
                                error_count = 0;
                                doubler = 1;
                            },
                            Err(e) => {
                                error_count += 1;
                                 if error_count >= max_retries {
                                    error!("MQTT event loop error: {e:?}. Encountered {error_count} consecutive errors, exceeding the maximum of {max_retries}. Stopping the event loop.");
                                    return Err(anyhow::anyhow!("MQTT event loop has encountered {error_count} consecutive errors, exceeding the maximum of {max_retries}. Stopping the event loop."));
                                } else {
                                    let delay = base_retry_delay_secs * doubler;
                                    info!("MQTT event loop error: {e:?}. Waiting for {delay} seconds before retrying...");
                                    tokio::time::sleep(Duration::from_secs(delay)).await;

                                    // refresh MQTT options security settings.
                                    self.set_mqtt_options_v3_security(&mut event_loop_v3.mqtt_options).await.unwrap_or_else(|e| {
                                        error!("Failed to update MQTT options with security credentials: {e:?}");
                                    });
                                    doubler *= 2; // exponential backoff
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    //....... Private helper methods

    async fn mqtt5_connect(
        id: impl Into<String>,
        config: &MqttSourceConfig,
        identity_provider: &Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> anyhow::Result<(AsyncClientV5, EventLoopV5)> {
        let options_v5 =
            Self::config_to_mqtt_options_v5(id, config, identity_provider.clone()).await?;

        let (client_v5, mut event_loop_v5) =
            AsyncClientV5::new(options_v5, config.event_channel_capacity);
        for trial in 0..5 {
            match event_loop_v5.poll().await {
                Ok(event) => {
                    info!("Successfully connected to MQTT broker using v5 options");
                    return Ok((client_v5, event_loop_v5));
                }
                Err(ConnectionError::ConnectionRefused(
                    ConnectReturnCode::UnsupportedProtocolVersion,
                )) => {
                    error!(
                        "Failed to connect using MQTT v5 options: Unsupported protocol version."
                    );
                    return Err(anyhow::anyhow!(
                        "Failed to connect using MQTT v5 options: Unsupported protocol version. The broker may not support MQTT v5."
                    ));
                }
                Err(e) => {
                    error!(
                        "Failed to connect using MQTT v5 options on trial {}: {:?}",
                        trial + 1,
                        e
                    );
                }
            };
            // delay before retrying
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(anyhow::anyhow!(
            "Failed to connect to MQTT broker using MQTT v5 options after multiple attempts."
        ))
    }

    async fn mqttv3_connect(
        id: impl Into<String>,
        config: &MqttSourceConfig,
        identity_provider: &Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> anyhow::Result<(AsyncClient, EventLoop)> {
        let options_v3 =
            Self::config_to_mqtt_options_v3(id, config, identity_provider.clone()).await?;
        let (client_v3, mut event_loop_v3) =
            AsyncClient::new(options_v3, config.event_channel_capacity);

        for trial in 0..5 {
            match event_loop_v3.poll().await {
                Ok(event) => {
                    info!("Successfully connected to MQTT broker using v3 options");
                    return Ok((client_v3, event_loop_v3));
                }
                Err(e) => {
                    error!(
                        "Failed to connect using MQTT v3 options on trial {}: {:?}",
                        trial + 1,
                        e
                    );
                }
            };
            // delay before retrying
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(anyhow::anyhow!(
            "Failed to connect to MQTT broker using MQTT v3 options after multiple attempts."
        ))
    }

    async fn subscribe_to_topics(&mut self, config: &MqttSourceConfig) -> anyhow::Result<()> {
        for topic_config in &config.topics {
            let topic = topic_config.topic.clone();
            match &mut self.client {
                MqttAsyncClientWrapper::AsyncClientV5(client_v5) => {
                    let qos = match topic_config.qos {
                        MqttQoS::ZERO => rumqttc::v5::mqttbytes::QoS::AtMostOnce,
                        MqttQoS::ONE => rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
                        MqttQoS::TWO => rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                    };

                    if let Some(subscribe_properties) = config.subscribe_properties.as_ref() {
                        client_v5.subscribe_with_properties(
                            topic.clone(),
                            qos,
                            subscribe_properties.to_subscribe_properties(),
                        ).await.map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to subscribe to topic '{topic}' with MQTT v5 client using properties: {e:?}",
                            )
                        })?;
                    } else {
                        client_v5.subscribe(topic.clone(), qos).await.map_err(|e| {
                            anyhow::anyhow!(
                                "Failed to subscribe to topic '{topic}' with MQTT v5 client: {e:?}",
                            )
                        })?;
                    }
                }
                MqttAsyncClientWrapper::AsyncClientV3(client_v3) => {
                    let qos = match topic_config.qos {
                        MqttQoS::ZERO => rumqttc::QoS::AtMostOnce,
                        MqttQoS::ONE => rumqttc::QoS::AtLeastOnce,
                        MqttQoS::TWO => rumqttc::QoS::ExactlyOnce,
                    };
                    client_v3.subscribe(topic.clone(), qos).await.map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to subscribe to topic '{topic}' with MQTT v3 client: {e:?}",
                        )
                    })?;
                }
            }
        }
        Ok(())
    }

    // needed if security tokens are expired
    async fn set_mqtt_options_v5_security(
        &self,
        mqtt_options: &mut MqttOptionsV5,
    ) -> anyhow::Result<()> {
        if let Some(config) = self.config.as_ref() {
            common_security_config_to_mqtt_options!(mqtt_options, config, &self.identity_provider); // calls the available macro
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "No configuration available to update MQTT options security settings"
            ))
        }
    }

    async fn set_mqtt_options_v3_security(
        &self,
        mqtt_options: &mut MqttOptions,
    ) -> anyhow::Result<()> {
        if let Some(config) = self.config.as_ref() {
            common_security_config_to_mqtt_options!(mqtt_options, config, &self.identity_provider); // calls the available macro
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "No configuration available to update MQTT options security settings"
            ))
        }
    }

    pub async fn config_to_mqtt_options_v5(
        id: impl Into<String>,
        config: &MqttSourceConfig,
        identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> anyhow::Result<MqttOptionsV5> {
        let mut options = MqttOptionsV5::new(id, config.host.clone(), config.port);

        // Common between v5 and v3.1.1
        common_config_to_mqtt_options!(options, config, identity_provider);

        if let Some(max_inflight) = config.max_inflight {
            options.set_outgoing_inflight_upper_limit(max_inflight);
        }
        if let Some(clean_start) = config.clean_start {
            options.set_clean_start(clean_start);
        }

        // v5 specific options
        if let Some(conn_timeout) = config.conn_timeout {
            options.set_connection_timeout(conn_timeout);
        }
        if let Some(connect_properties) = config.connect_properties.as_ref() {
            let connection_props = connect_properties.to_connection_properties();
            options.set_connect_properties(connection_props);
        }

        // Subscribe properties are set during subscription, not connection, so they are handled in the subscribe_to_topics method

        Ok(options)
    }

    pub async fn config_to_mqtt_options_v3(
        id: impl Into<String>,
        config: &MqttSourceConfig,
        identity_provider: Option<Arc<dyn drasi_lib::identity::IdentityProvider>>,
    ) -> anyhow::Result<MqttOptions> {
        let mut options = MqttOptions::new(id, config.host.clone(), config.port);

        // Common between v5 and v3.1.1
        common_config_to_mqtt_options!(options, config, identity_provider);

        if let Some(max_inflight) = config.max_inflight {
            options.set_inflight(max_inflight);
        }
        if let Some(clean_start) = config.clean_start {
            options.set_clean_session(clean_start);
        }

        // v3 specific options
        if let Some((incoming, outgoing)) = config.max_packet_sizes() {
            options.set_max_packet_size(incoming, outgoing);
        }

        Ok(options)
    }
}
