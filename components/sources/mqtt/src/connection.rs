use core::panic;
use std::{option, time::Duration};

use crate::config::{MqttQoS, MqttTransportMode};
use crate::{config::MQTTSourceConfig, mqtt};
use rumqttc::v5::{
    AsyncClient as AsyncClientV5, EventLoop as EventLoopV5, MqttOptions as MqttOptionsV5,
};
use rumqttc::{
    v5::{mqttbytes::v5::ConnectReturnCode, ConnectionError},
    AsyncClient, EventLoop, MqttOptions,
};
use tracing::event;

use log::{debug, error, info, trace, warn};

pub enum MqttAsyncClientWrapper {
    AsyncClientV3(AsyncClient),
    AsyncClientV5(AsyncClientV5),
}

enum MqttEventLoopWrapper {
    EventLoopV3 { event_loop: EventLoop },
    EventLoopV5 { event_loop: EventLoopV5 },
}
pub(crate) struct MqttConnection {
    client: MqttAsyncClientWrapper,
    event_loop_handle: Option<tokio::task::JoinHandle<()>>,
}

impl MqttConnection {
    //....... Public methods

    pub async fn new(
        id: impl Into<String>,
        config: &MQTTSourceConfig,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>, // to handle multiple topics in future. TODO
    ) -> anyhow::Result<Self> {
        let id_v5 = id.into().clone();
        let id_v3 = id_v5.clone();
        // try Mqtt v5 first
        let options_v5 = Self::config_to_mqtt_options_v5(id_v5, config);
        let (client_v5, mut event_loop_v5) =
            AsyncClientV5::new(options_v5, config.event_channel_capacity);
        match event_loop_v5.poll().await {
            Ok(event) => {
                info!("Successfully connected to MQTT broker using v5 options");
                let mut connection = Self {
                    client: MqttAsyncClientWrapper::AsyncClientV5(client_v5),
                    event_loop_handle: None,
                };
                connection
                    .run_subscription_loop(
                        shutdown_rx,
                        MqttEventLoopWrapper::EventLoopV5 {
                            event_loop: event_loop_v5,
                        },
                        config,
                    )
                    .await;
                return Ok(connection);
            }
            Err(ConnectionError::ConnectionRefused(
                ConnectReturnCode::UnsupportedProtocolVersion,
            )) => {
                error!("Failed to connect using MQTT v5 options: Unsupported protocol version. Attempting MQTT v3 fallback...");
            }
            Err(e) => {
                error!(
                    "Failed to connect using MQTT v5 options: {:?}. Attempting MQTT v3 fallback...",
                    e
                );
            }
        };

        // fallback to Mqtt v3
        let options_v3 = Self::config_to_mqtt_options_v3(id_v3, config);
        let (client_v3, mut event_loop_v3) =
            AsyncClient::new(options_v3, config.event_channel_capacity);
        match event_loop_v3.poll().await {
            Ok(event) => {
                info!("Successfully connected to MQTT broker using v3 options");
                let mut connection = Self {
                    client: MqttAsyncClientWrapper::AsyncClientV3(client_v3),
                    event_loop_handle: None,
                };
                connection
                    .run_subscription_loop(
                        shutdown_rx,
                        MqttEventLoopWrapper::EventLoopV3 {
                            event_loop: event_loop_v3,
                        },
                        config,
                    )
                    .await;
                return Ok(connection);
            }
            Err(e) => {
                error!(
                    "Failed to connect using MQTT v3 options: {:?}. No more fallbacks available.",
                    e
                );
            }
        };

        Err(anyhow::anyhow!("Failed to create MQTT client with both v5 and v3 options. Check configuration for errors."))
    }

    //....... Private helper methods

    async fn run_subscription_loop(
        &mut self,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
        mut event_loop: MqttEventLoopWrapper,
        config: &MQTTSourceConfig,
    ) {
        self.subscribe_to_topics(config).await.unwrap_or_else(|e| {
            error!("Failed to subscribe to topics: {:?}", e);
            return;
        });

        let mqtt_version: u8 = match &self.client {
            MqttAsyncClientWrapper::AsyncClientV5(_) => 5,
            MqttAsyncClientWrapper::AsyncClientV3(_) => 3,
        };

        self.event_loop_handle = Some(tokio::spawn(async move {
            if mqtt_version == 5 {
                let mut event_loop_v5 = match event_loop {
                    MqttEventLoopWrapper::EventLoopV5 { event_loop } => event_loop,
                    _ => {
                        error!("Expected MQTT v5 event loop");
                        return;
                    }
                };
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            info!("Shutdown signal received, stopping MQTT event loop");
                            break;
                        },
                        event = event_loop_v5.poll() => {
                            match event {
                                Ok(event) => {
                                    info!("MQTT v5 event: {:?}", event);
                                },
                                Err(e) => {
                                    error!("MQTT v5 event loop error: {:?}", e);
                                }
                            }
                        }
                    }
                }
            } else {
                let mut event_loop_v3 = match event_loop {
                    MqttEventLoopWrapper::EventLoopV3 { event_loop } => event_loop,
                    _ => {
                        error!("Expected MQTT v3 event loop");
                        return;
                    }
                };
                loop {
                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            info!("Shutdown signal received, stopping MQTT event loop");
                            break;
                        },
                        event = event_loop_v3.poll() => {
                            match event {
                                Ok(event) => {
                                    info!("MQTT v3 event: {:?}", event);
                                },
                                Err(e) => {
                                    error!("MQTT v3 event loop error: {:?}", e);
                                }
                            }
                        }
                    }
                }
            }
        }))
    }

    async fn subscribe_to_topics(&mut self, config: &MQTTSourceConfig) -> anyhow::Result<()> {
        for topic_config in &config.topics {
            let topic = topic_config.topic.clone();
            match &mut self.client {
                MqttAsyncClientWrapper::AsyncClientV5(client_v5) => {
                    let qos = match topic_config.qos {
                        MqttQoS::ZERO => rumqttc::v5::mqttbytes::QoS::AtMostOnce,
                        MqttQoS::ONE => rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
                        MqttQoS::TWO => rumqttc::v5::mqttbytes::QoS::ExactlyOnce,
                    };
                    client_v5.subscribe(topic.clone(), qos).await.map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to subscribe to topic '{}' with MQTT v5 client: {:?}",
                            topic,
                            e
                        )
                    })?;
                }
                MqttAsyncClientWrapper::AsyncClientV3(client_v3) => {
                    let qos = match topic_config.qos {
                        MqttQoS::ZERO => rumqttc::QoS::AtMostOnce,
                        MqttQoS::ONE => rumqttc::QoS::AtLeastOnce,
                        MqttQoS::TWO => rumqttc::QoS::ExactlyOnce,
                    };
                    client_v3.subscribe(topic.clone(), qos).await.map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to subscribe to topic '{}' with MQTT v3 client: {:?}",
                            topic,
                            e
                        )
                    })?;
                }
            }
        }
        Ok(())
    }

    fn config_to_mqtt_options_v5(
        id: impl Into<String>,
        config: &MQTTSourceConfig,
    ) -> MqttOptionsV5 {
        let mut options = MqttOptionsV5::new(id, config.broker_addr.clone(), config.port);

        // Common between v5 and v3.1.1
        if let Some(tranport) = config.transport.as_ref() {
            match tranport {
                crate::config::MqttTransportMode::TLS {
                    ca,
                    alpn,
                    client_auth,
                } => {
                    let tls_config = rumqttc::TlsConfiguration::Simple {
                        ca: ca.clone(),
                        alpn: alpn.clone(),
                        client_auth: client_auth.clone(),
                    };
                    options.set_transport(rumqttc::Transport::Tls(tls_config));
                }
                _ => {}
            }
        }

        if let Some(request_channel_capacity) = config.request_channel_capacity {
            options.set_request_channel_capacity(request_channel_capacity);
        }

        if let Some(max_inflight) = config.max_inflight {
            options.set_outgoing_inflight_upper_limit(max_inflight);
        }

        if let Some(keep_alive) = config.keep_alive {
            options.set_keep_alive(Duration::from_secs(keep_alive));
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

        options
    }

    fn config_to_mqtt_options_v3(id: impl Into<String>, config: &MQTTSourceConfig) -> MqttOptions {
        let mut options = MqttOptions::new(id, config.broker_addr.clone(), config.port);

        // Common between v5 and v3.1.1
        if let Some(tranport) = config.transport.as_ref() {
            match tranport {
                crate::config::MqttTransportMode::TLS {
                    ca,
                    alpn,
                    client_auth,
                } => {
                    let tls_config = rumqttc::TlsConfiguration::Simple {
                        ca: ca.clone(),
                        alpn: alpn.clone(),
                        client_auth: client_auth.clone(),
                    };
                    options.set_transport(rumqttc::Transport::Tls(tls_config));
                }
                _ => {}
            }
        }

        if let Some(request_channel_capacity) = config.request_channel_capacity {
            options.set_request_channel_capacity(request_channel_capacity);
        }

        if let Some(max_inflight) = config.max_inflight {
            options.set_inflight(max_inflight);
        }

        if let Some(keep_alive) = config.keep_alive {
            options.set_keep_alive(Duration::from_secs(keep_alive));
        }

        if let Some(clean_start) = config.clean_start {
            options.set_clean_session(clean_start);
        }

        // v3 specific options
        if let Some(max_incoming_packet_size) = config.max_incoming_packet_size {
            options.set_max_packet_size(max_incoming_packet_size, max_incoming_packet_size);
            // TODO
        }

        if let Some(max_outgoing_packet_size) = config.max_outgoing_packet_size {
            options.set_max_packet_size(max_outgoing_packet_size, max_outgoing_packet_size);
            // TODO
        }

        options
    }
}
