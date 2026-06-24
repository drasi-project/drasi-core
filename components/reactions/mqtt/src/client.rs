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

use crate::config::MqttQoS;
use crate::config::{MqttProtocolVersion, MqttReactionConfig};
use crate::processor::MqttEventLoop;
use crate::verifier::NoVerifier;
use anyhow::Result;
use log::error;
use rumqttc::v5::{
    mqttbytes::{v5::PublishProperties, QoS as QosV5},
    AsyncClient as AsyncClientV5, MqttOptions as MqttOptionsV5,
};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use rustls::ClientConfig;
use rustls::RootCertStore;
use std::sync::Arc;
use std::time::Duration;

macro_rules! validate_identity_provider {
    ($config:expr, $options:expr, $host:expr, $port:expr) => {
        // add identity provider credentials if provided
        if let Some(identity_provider) = &$config.identity_provider {
            let context = drasi_lib::identity::CredentialContext::new()
                .with_property("hostname", &$host)
                .with_property("port", $port.to_string());
            let cred = identity_provider.get_credentials(&context).await;
            match cred {
                Ok(cred) => match cred {
                    drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
                        $options.set_credentials(username, password);
                    }
                    _ => {
                        error!("Unsupported credential type from identity provider, expected username/password");
                        return Err(anyhow::anyhow!(
                            "Unsupported credential type from identity provider, deferred to v2"
                        ));
                    }
                },
                Err(e) => {
                    error!("Failed to get credentials from identity provider: {e:?}");
                    return Err(anyhow::anyhow!(
                        "Failed to get credentials from identity provider: {e:?}"
                    ));
                }
            }
        }
    };
}

macro_rules! validate_tls {
    ($config:expr, $options:expr) => {
        // set TLS options if provided (mTLS is not supported)
        if let Some(transport) = $config.tls.as_ref() {
            let mut client_config = if transport.accept_invalid_certs {
                ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerifier))
                    .with_no_client_auth()
            } else {
                let mut store: RootCertStore = RootCertStore::empty();
                match &transport.ca {
                    None => {
                        // using system CA store
                        let certs = rustls_native_certs::load_native_certs().certs;
                        for cert in certs {
                            store.add(cert)?;
                        }
                    }
                    Some(pem_bytes) => {
                        // using custom-user defined CA certs
                        let certs = rustls_pemfile::certs(&mut &pem_bytes[..])
                            .collect::<Result<Vec<_>, _>>()?;
                        for cert in certs {
                            store.add(cert)?;
                        }
                    }
                }

                ClientConfig::builder()
                    .with_root_certificates(store)
                    .with_no_client_auth()
            };

            if let Some(alpn) = &transport.alpn {
                client_config.alpn_protocols = alpn.clone();
            }

            $options.set_transport(rumqttc::Transport::tls_with_config(
                rumqttc::TlsConfiguration::Rustls(Arc::new(client_config)),
            ));
        }
    };
}

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
    async fn publish(
        &self,
        topic: &str,
        qos: MqttQoS,
        retain: bool,
        payload: String,
        message_expiry_interval: Option<u32>,
    ) -> Result<()>;
}

impl Publish for MqttAsyncClient {
    async fn publish(
        &self,
        topic: &str,
        qos: MqttQoS,
        retain: bool,
        payload: String,
        message_expiry_interval: Option<u32>,
    ) -> Result<()> {
        match self {
            MqttAsyncClient::V5(client) => {
                // set the hardcoded (V1) properties for v5 publish
                let publish_properties = PublishProperties {
                    content_type: Some("application/json".to_string()),
                    payload_format_indicator: Some(1),
                    message_expiry_interval,
                    ..Default::default()
                };

                client
                    .publish_with_properties(
                        topic,
                        qos.to_rumqttc_qos_v5(),
                        retain,
                        payload,
                        publish_properties,
                    )
                    .await?;
                Ok(())
            }
            MqttAsyncClient::V3_1_1(client) => {
                client
                    .publish(topic, qos.to_rumqttc_qos_v3_1_1(), retain, payload)
                    .await?;
                Ok(())
            }
        }
    }
}

pub(crate) struct Client {
    client: MqttAsyncClient,
}

impl Client {
    pub async fn new(
        id: String,
        config: &MqttReactionConfig,
    ) -> anyhow::Result<(Self, MqttEventLoop)> {
        match config.protocol_version {
            MqttProtocolVersion::V5 => {
                let mqtt_options_v5 = Self::config_to_mqtt_options_v5(&id, config).await?;
                let (client, event_loop) =
                    AsyncClientV5::new(mqtt_options_v5, config.event_channel_capacity);
                Ok((
                    Self {
                        client: MqttAsyncClient::V5(client),
                    },
                    MqttEventLoop::V5(event_loop),
                ))
            }
            MqttProtocolVersion::V3_1_1 => {
                let mqtt_options_v3_1_1 = Self::config_to_mqtt_options_v3_1_1(&id, config).await?;
                let (client, event_loop) =
                    AsyncClient::new(mqtt_options_v3_1_1, config.event_channel_capacity);
                Ok((
                    Self {
                        client: MqttAsyncClient::V3_1_1(client),
                    },
                    MqttEventLoop::V3_1_1(event_loop),
                ))
            }
        }
    }

    /// Publish a message to the MQTT broker with the specified topic, payload, QoS, and retain flag.
    pub async fn publish(
        &self,
        topic: String,
        qos: MqttQoS,
        retain: bool,
        payload: String,
        message_expiry_interval: Option<u32>,
    ) -> Result<()> {
        self.client
            .publish(&topic, qos, retain, payload, message_expiry_interval)
            .await?;
        Ok(())
    }
    /// Translate the MqttReactionConfig into rumqttc::MqttOptions for v5
    async fn config_to_mqtt_options_v5(
        id: &str,
        config: &MqttReactionConfig,
    ) -> anyhow::Result<MqttOptionsV5> {
        // parse the URL to extract host and port
        let (host, port) = Self::parse_url(config)?;

        // generate MQTT options based on the config
        let mut options = MqttOptionsV5::new(id, host.clone(), port);

        // validate identity provider and TLS settings
        validate_identity_provider!(config, options, host, port);
        validate_tls!(config, options);

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

        options.set_session_expiry_interval(config.session_expiry_interval);

        Ok(options)
    }

    /// Translate the MqttReactionConfig into rumqttc::MqttOptions for v3.1.1
    async fn config_to_mqtt_options_v3_1_1(
        id: &str,
        config: &MqttReactionConfig,
    ) -> anyhow::Result<MqttOptions> {
        // parse the URL to extract host and port
        let (host, port) = Self::parse_url(config)?;

        // generate MQTT options based on the config
        let mut options = MqttOptions::new(id, host.clone(), port);

        // validate identity provider and TLS settings
        validate_identity_provider!(config, options, host, port);
        validate_tls!(config, options);

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
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid URL: missing host"))?
            .to_string();
        let port = url.port_or_known_default().ok_or_else(|| {
            anyhow::anyhow!("Invalid URL: missing port and no default for scheme")
        })?;
        Ok((host, port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};

    #[derive(Clone)]
    struct TokenIdentityProvider;

    #[async_trait]
    impl IdentityProvider for TokenIdentityProvider {
        async fn get_credentials(
            &self,
            _context: &CredentialContext,
        ) -> anyhow::Result<Credentials> {
            Ok(Credentials::Token {
                username: "token-user".to_string(),
                token: "secret-token".to_string(),
            })
        }

        fn clone_box(&self) -> Box<dyn IdentityProvider> {
            Box::new(self.clone())
        }
    }

    fn base_config(url: &str) -> MqttReactionConfig {
        MqttReactionConfig {
            url: url.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn qos_mapping_for_v3_1_1_is_correct() {
        assert_eq!(MqttQoS::AtMostOnce.to_rumqttc_qos_v3_1_1(), QoS::AtMostOnce);
        assert_eq!(
            MqttQoS::AtLeastOnce.to_rumqttc_qos_v3_1_1(),
            QoS::AtLeastOnce
        );
    }

    #[test]
    fn qos_mapping_for_v5_is_correct() {
        assert_eq!(MqttQoS::AtMostOnce.to_rumqttc_qos_v5(), QosV5::AtMostOnce);
        assert_eq!(MqttQoS::AtLeastOnce.to_rumqttc_qos_v5(), QosV5::AtLeastOnce);
    }

    #[test]
    fn parse_url_accepts_supported_schemes_with_explicit_ports() {
        let cases = [
            ("mqtt://broker.example.com:1883", 1883_u16),
            ("mqtts://broker.example.com:8883", 8883_u16),
            ("ws://broker.example.com:80", 80_u16),
            ("wss://broker.example.com:443", 443_u16),
        ];

        for (url, expected_port) in cases {
            let config = base_config(url);
            let (host, port) = Client::parse_url(&config).expect("url should parse");
            assert_eq!(host, "broker.example.com");
            assert_eq!(port, expected_port);
        }
    }

    #[test]
    fn parse_url_returns_error_when_port_is_missing() {
        let config = base_config("mqtt://broker.example.com");
        let err = Client::parse_url(&config).expect_err("portless url should fail");
        assert!(err.to_string().contains("missing port"));
    }

    #[test]
    fn parse_url_returns_error_when_host_is_missing() {
        let config = base_config("mqtt://:1883");
        let err = Client::parse_url(&config).expect_err("hostless url should fail");
        assert!(
            err.to_string().contains("missing host")
                || err.to_string().to_ascii_lowercase().contains("empty host")
        );
    }

    #[tokio::test]
    async fn new_returns_v5_event_loop_for_v5_protocol() {
        let mut config = base_config("mqtt://localhost:1883");
        config.protocol_version = MqttProtocolVersion::V5;

        let (_client, event_loop) = Client::new("test-client-v5".to_string(), &config)
            .await
            .expect("client creation should succeed");

        assert!(matches!(event_loop, MqttEventLoop::V5(_)));
    }

    #[tokio::test]
    async fn new_returns_v3_1_1_event_loop_for_v3_1_1_protocol() {
        let mut config = base_config("mqtt://localhost:1883");
        config.protocol_version = MqttProtocolVersion::V3_1_1;

        let (_client, event_loop) = Client::new("test-client-v3".to_string(), &config)
            .await
            .expect("client creation should succeed");

        assert!(matches!(event_loop, MqttEventLoop::V3_1_1(_)));
    }

    #[tokio::test]
    async fn config_to_mqtt_options_v5_rejects_non_username_password_credentials() {
        let mut config = base_config("mqtt://localhost:1883");
        config.identity_provider = Some(Box::new(TokenIdentityProvider));

        let err = Client::config_to_mqtt_options_v5("client-v5", &config)
            .await
            .expect_err("token credentials should be rejected");

        assert!(err
            .to_string()
            .contains("Unsupported credential type from identity provider"));
    }

    #[tokio::test]
    async fn config_to_mqtt_options_v3_1_1_rejects_non_username_password_credentials() {
        let mut config = base_config("mqtt://localhost:1883");
        config.identity_provider = Some(Box::new(TokenIdentityProvider));

        let err = Client::config_to_mqtt_options_v3_1_1("client-v3", &config)
            .await
            .expect_err("token credentials should be rejected");

        assert!(err
            .to_string()
            .contains("Unsupported credential type from identity provider"));
    }
}
