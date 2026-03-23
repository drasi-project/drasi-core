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

use crate::config::MqttReactionConfig;
use crate::config::{MqttCallSpec, RetainPolicy};
use crate::MqttAuthMode;
use crate::MqttTransportMode;
use anyhow::Result;
use drasi_lib::reactions::ReactionBase;
use drasi_lib::reactions::ReactionBaseParams;
use log::{error, info, warn};
use rumqttc::v5::{mqttbytes::QoS, AsyncClient, Event, EventLoop, Incoming, MqttOptions};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;

pub struct MqttClient {
    pub(crate) client_id: String,
    client: AsyncClient,
    event_loop_handle: Option<tokio::task::JoinHandle<()>>,
}

impl MqttClient {
    pub async fn new(
        id: impl Into<String>,
        config: &MqttReactionConfig,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    ) -> Result<Self> {
        let reaction_name = id.into();
        let mqttoptions = match Self::generate_mqtt_options(reaction_name.clone(), config) {
            Ok(options) => options,
            Err(e) => {
                error!("[{reaction_name}] Failed to generate MQTT options: {e:?}");
                anyhow::bail!("Failed to generate MQTT options: {e:?}");
            }
        };

        let (client, event_loop) = AsyncClient::new(mqttoptions, config.event_channel_capacity);

        let mut mqtt_client = Self {
            client_id: reaction_name,
            client,
            event_loop_handle: None,
        };

        mqtt_client.start_event_loop(shutdown_rx, event_loop).await;

        Ok(mqtt_client)
    }

    pub async fn start_event_loop(
        &mut self,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
        mut eventloop: EventLoop,
    ) {
        let reaction_name_eventloop = self.client_id.clone();
        self.event_loop_handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("[{reaction_name_eventloop}] Received MQTT event loop shutdown signal, exiting event loop");
                        break;
                    },
                    event = eventloop.poll() => {
                        match event {
                            Ok(_) => {},
                            Err(e) => {
                                warn!("[{reaction_name_eventloop}] MQTT event loop error: {e}");
                            }
                        }
                    }
                }
            }
        }))
    }

    pub async fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: QoS,
        retain: bool,
    ) -> Result<()> {
        self.client.publish(topic, qos, retain, payload).await?;
        Ok(())
    }

    pub async fn publish_from_call_spec(
        &self,
        call_spec: &MqttCallSpec,
        body: String,
    ) -> Result<()> {
        let topic = call_spec.topic.clone();
        let qos = match call_spec.qos {
            crate::config::QualityOfService::AtMostOnce => QoS::AtMostOnce,
            crate::config::QualityOfService::AtLeastOnce => QoS::AtLeastOnce,
            crate::config::QualityOfService::ExactlyOnce => QoS::ExactlyOnce,
        };

        let retain = match call_spec.retain {
            RetainPolicy::Retain => true,
            RetainPolicy::NoRetain => false,
        };

        self.publish(&topic, body.as_bytes().to_vec(), qos, retain)
            .await
    }

    fn generate_mqtt_options(
        reaction_name: impl Into<String>,
        config: &MqttReactionConfig,
    ) -> Result<MqttOptions> {
        let broker_addr = config.broker_addr.clone();
        let port = config.port;
        let transport_mode = config.transport_mode.clone();
        let keep_alive = config.keep_alive;
        let clean_session = config.clean_session;
        let auth_mode = config.auth_mode.clone();
        let request_channel_capacity = config.request_channel_capacity;
        let pending_throttle = config.pending_throttle;
        let max_packet_size = config.max_packet_size;
        let max_inflight = config.max_inflight;
        let connection_timeout = config.connection_timeout;

        let mut options = MqttOptions::new(reaction_name.into(), broker_addr.clone(), port);

        match transport_mode {
            MqttTransportMode::TCP => {
                options.set_transport(rumqttc::Transport::Tcp);
            }
            MqttTransportMode::TLS => {
                // TODO: Add TLS configuration options to MqttReactionConfig and set them here
                anyhow::bail!("TLS transport mode is not yet supported");
            }
        }

        if let MqttAuthMode::UsernamePassword { username, password } = auth_mode {
            options.set_credentials(username, password);
        }

        options.set_outgoing_inflight_upper_limit(max_inflight);
        options.set_keep_alive(Duration::from_secs(keep_alive));
        options.set_clean_start(clean_session);
        options.set_max_packet_size(Some(max_packet_size));
        options.set_pending_throttle(Duration::from_micros(pending_throttle));
        options.set_request_channel_capacity(request_channel_capacity);
        options.set_connection_timeout(connection_timeout);

        Ok(options)
    }
}
