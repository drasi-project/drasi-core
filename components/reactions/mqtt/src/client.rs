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

use crate::config::MqttQoS;
use crate::config::{MqttProtocolVersion, MqttReactionConfig};
use anyhow::Result;
use drasi_lib::reactions::ReactionBase;
use drasi_lib::reactions::ReactionBaseParams;
use log::{error, info, warn};
use rumqttc::v5::{mqttbytes::QoS as QosV5, AsyncClient as AsyncClientV5, Event, EventLoop as EventLoopV5, Incoming, MqttOptions as MqttOptionsV5};
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
use rustls::ClientConfig;
use core::error;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use rustls::{RootCertStore};
use rustls::HandshakeType::Certificate;
use crate::verifier::NoVerifier;
use rustls_native_certs;
use rustls_pemfile::certs;

impl MqttQoS {
    pub fn to_rumqttc_qos_v3_1_1(&self) -> QoS {
        match self {
            MqttQoS::AtMostOnce => QoS::AtMostOnce,
            MqttQoS::AtLeastOnce => QoS::AtLeastOnce,
        }
    }

    pub fn to_rumqttc_qos_v5(&self) -> QosV5 {
        match self {
            MqttQoS::AtMostOnce => QosV5::AtMostOnce,
            MqttQoS::AtLeastOnce => QosV5::AtLeastOnce,
        }
    }
    
}

enum MqttAsyncClient {
    V5(AsyncClientV5),
    V3_1_1(AsyncClient),
}

trait Publish {
    async fn publish(&self, topic: &str, payload: Vec<u8>, qos: MqttQoS, retain: bool) -> Result<()>;
}

impl Publish for MqttAsyncClient {
    async fn publish(&self, topic: &str, payload: Vec<u8>, qos: MqttQoS, retain: bool) -> Result<()> {
        match self {
            MqttAsyncClient::V5(client) => {
                client.publish(topic, qos.to_rumqttc_qos_v5(), retain, payload).await?;
                Ok(())
            }
            MqttAsyncClient::V3_1_1(client) => {
                client.publish(topic, qos.to_rumqttc_qos_v3_1_1(), retain, payload).await?;
                Ok(())
            }
        }
    }
}

pub struct Client {
    pub(crate) client_id: String,
    client: MqttAsyncClient,
}

impl Client {
    pub async fn new(
        config: &MqttReactionConfig,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    ) -> Result<(Self, tokio::task::JoinHandle<()>)> {

        match config.protocol_version {
            MqttProtocolVersion::V5 => {
                let mqtt_options_v5 = Self::config_to_mqtt_options_v5(config).await?;
                let (client, event_loop) = AsyncClientV5::new(mqtt_options_v5, config.event_channel_capacity);
                let client_id = config.client_id.clone().unwrap_or_else(|| "".to_string());
                let event_loop_handle = Self::start_event_loop_v5(client_id.clone(), event_loop, shutdown_rx).await?;
                Ok((Self { client_id, client: MqttAsyncClient::V5(client) }, event_loop_handle))
            }
            MqttProtocolVersion::V3_1_1 => {
                let mqtt_options_v3_1_1 = Self::config_to_mqtt_options_v3_1_1(config).await?;
                let (client, event_loop) = AsyncClient::new(mqtt_options_v3_1_1, config.event_channel_capacity);
                let client_id = config.client_id.clone().unwrap_or_else(|| "".to_string());
                let event_loop_handle = Self::start_event_loop_v3_1_1(client_id.clone(), event_loop, shutdown_rx).await?;
                Ok((Self { client_id, client: MqttAsyncClient::V3_1_1(client) }, event_loop_handle))
            }
        }
    }

    async fn start_event_loop_v3_1_1(
        client_id: String,
        mut event_loop: EventLoop,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("[{client_id}] Received MQTT event loop shutdown signal, exiting event loop");
                        break;
                    },
                    event = event_loop.poll() => {
                        match event {
                            Ok(_) => {},
                            Err(e) => {
                                warn!("[{client_id}] MQTT event loop error: {e}");
                            }
                        }
                    }
                }
            }
        });

        Ok(handle)
    }

    async fn start_event_loop_v5(
        client_id: String,
        mut event_loop: EventLoopV5,
        mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                    info!("[{client_id}] Received MQTT event loop shutdown signal, exiting event loop");
                    break;
                    },
                    event = event_loop.poll() => {
                        match event {
                            Ok(_) => {},
                            Err(e) => {
                                warn!("[{client_id}] MQTT event loop error: {e}");
                            }
                        }
                    }
                }
            }
        });
        Ok(handle)
    }

    pub async fn publish(
        &self,
        topic: &str,
        payload: Vec<u8>,
        qos: MqttQoS,
        retain: bool,
    ) -> Result<()> {
        self.client.publish(topic, payload, qos, retain).await?;
        Ok(())
    }

    pub async fn config_to_mqtt_options_v5(
        config: &MqttReactionConfig,
    ) -> anyhow::Result<MqttOptionsV5> {

        // parse the URL to extract host and port
        let (host, port) = Self::parse_url(config)?;

        // generate MQTT options based on the config
        let mut options = if let Some(client_id) = &config.client_id {
            MqttOptionsV5::new(client_id.clone(), host.clone(), port)
        } else {
            MqttOptionsV5::new("", host.clone(), port)
        };

        // add identity provider credentials if provided
        if let Some(identity_provider) = &config.identity_provider {
            let context = drasi_lib::identity::CredentialContext::new()
                .with_property("hostname", &host)
                .with_property("port", &port.to_string());
            let cred = identity_provider.get_credentials(&context).await;
            match cred {
                Ok(cred) => {
                    match cred {
                        drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
                            options.set_credentials(username, password);
                        }
                        _ => {
                            error!("Unsupported credential type from identity provider, expected username/password");
                            return Err(anyhow::anyhow!("Unsupported credential type from identity provider, deferred to v2"));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to get credentials from identity provider: {e:?}");
                    return Err(anyhow::anyhow!("Failed to get credentials from identity provider: {e:?}"));
                }
            }
        }

        // set TLS options if provided (mTLS is not supported)
        if let Some(trasnport) = config.tls.as_ref() {

            let mut client_config = if trasnport.accept_invalid_certs {
                ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerifier))
                    .with_no_client_auth()
            }
            else {

                let mut store: RootCertStore = RootCertStore::empty();
                match &trasnport.ca {
                    None => {
                        // using system CA store
                        let certs = rustls_native_certs::load_native_certs().certs;
                        for cert in  certs {
                            store.add(cert)?;
                        }
                    },
                    Some(pem_bytes) => {
                        // using custom-user defined CA certs
                        let certs = rustls_pemfile::certs(&mut &pem_bytes[..]).collect::<Result<Vec<_>, _>>()?;
                        for cert in certs {
                            store.add(cert)?;
                        }
                    }
                }
                
                ClientConfig::builder()
                    .with_root_certificates(store)
                    .with_no_client_auth()
            };

            if let Some(alpn) = &trasnport.alpn {
                client_config.alpn_protocols = alpn.clone();
            }

            options.set_transport(
                rumqttc::Transport::tls_with_config(
                    rumqttc::TlsConfiguration::Rustls(
                        Arc::new(client_config),
                    )
                )
            );
        }

        if let Some(keep_alive) = config.keep_alive {
            options.set_keep_alive(Duration::from_secs(keep_alive));
        }

        if let Some(max_inflight) = config.max_inflight {
            options.set_outgoing_inflight_upper_limit(max_inflight);
        }

        if let Some(clean_start) = config.clean_start {
            options.set_clean_start(clean_start);
        }

        if let Some(conn_timeout) = config.conn_timeout {
            options.set_connection_timeout(conn_timeout);
        }

        Ok(options)
    }


    pub async fn config_to_mqtt_options_v3_1_1(
        config: &MqttReactionConfig,
    ) -> anyhow::Result<MqttOptions> {

        // parse the URL to extract host and port
        let (host, port) = Self::parse_url(config)?;

        // generate MQTT options based on the config
        let mut options = if let Some(client_id) = &config.client_id {
            MqttOptions::new(client_id.clone(), host.clone(), port)
        } else {
            MqttOptions::new("", host.clone(), port)
        };

        // add identity provider credentials if provided
        if let Some(identity_provider) = &config.identity_provider {
            let context = drasi_lib::identity::CredentialContext::new()
                .with_property("hostname", &host)
                .with_property("port", &port.to_string());
            let cred = identity_provider.get_credentials(&context).await;
            match cred {
                Ok(cred) => {
                    match cred {
                        drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
                            options.set_credentials(username, password);
                        }
                        _ => {
                            error!("Unsupported credential type from identity provider, expected username/password");
                            return Err(anyhow::anyhow!("Unsupported credential type from identity provider, deferred to v2"));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to get credentials from identity provider: {e:?}");
                    return Err(anyhow::anyhow!("Failed to get credentials from identity provider: {e:?}"));
                }
            }
        }

        // set TLS options if provided (mTLS is not supported)
        if let Some(trasnport) = config.tls.as_ref() {

            let mut client_config = if trasnport.accept_invalid_certs {
                ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerifier))
                    .with_no_client_auth()
            }
            else {

                let mut store: RootCertStore = RootCertStore::empty();
                match &trasnport.ca {
                    None => {
                        // using system CA store
                        let certs = rustls_native_certs::load_native_certs().certs;
                        for cert in  certs {
                            store.add(cert)?;
                        }
                    },
                    Some(pem_bytes) => {
                        // using custom-user defined CA certs
                        let certs = rustls_pemfile::certs(&mut &pem_bytes[..]).collect::<Result<Vec<_>, _>>()?;
                        for cert in certs {
                            store.add(cert)?;
                        }
                    }
                }
                
                ClientConfig::builder()
                    .with_root_certificates(store)
                    .with_no_client_auth()
            };

            if let Some(alpn) = &trasnport.alpn {
                client_config.alpn_protocols = alpn.clone();
            }

            options.set_transport(
                rumqttc::Transport::tls_with_config(
                    rumqttc::TlsConfiguration::Rustls(
                        Arc::new(client_config),
                    )
                )
            );
        }

        if let Some(keep_alive) = config.keep_alive {
            options.set_keep_alive(Duration::from_secs(keep_alive));
        }

        if let Some(max_inflight) = config.max_inflight {
            options.set_inflight(max_inflight);
        }

        if let Some(clean_start) = config.clean_start {
            options.set_clean_session(clean_start);
        }

        Ok(options)
    }

    fn parse_url(config: &MqttReactionConfig) -> anyhow::Result<(String, u16)> {
        let url = url::Url::parse(&config.url)?;
        let host = url.host_str().ok_or_else(|| anyhow::anyhow!("Invalid URL: missing host"))?.to_string();
        let port = url.port_or_known_default().ok_or_else(|| anyhow::anyhow!("Invalid URL: missing port and no default for scheme"))?;
        Ok((host, port))
    }
}
