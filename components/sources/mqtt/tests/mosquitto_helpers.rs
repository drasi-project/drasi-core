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

//! Mosquitto test helpers for MQTT integration tests.
//!
//! Provides utilities to start and manage a Mosquitto MQTT broker using testcontainers.

use anyhow::{anyhow, Result};
use drasi_source_mqtt::config::{MqttQoS, MqttSourceConfig, MqttTopicConfig, MqttTransportMode};
use log::{debug, info};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};
use rumqttc::v5::{AsyncClient as AsyncClientV5, MqttOptions as MqttOptionsV5};
use rumqttc::{AsyncClient, MqttOptions, QoS};
use std::sync::Arc;
use std::time::Duration;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub struct MosquittoConfig {
    pub listener: u16,
    pub allow_anonymous: Option<bool>,
    pub protocols: Vec<String>,

    pub username: Option<String>,
    pub password: Option<String>,

    pub require_certificate: Option<bool>,
    pub ca: Option<String>,
    pub server_cert: Option<String>,
    pub server_key: Option<String>,
    pub use_identity_as_username: Option<bool>,
}

impl MosquittoConfig {
    pub fn new() -> Self {
        Self {
            listener: 1883,
            allow_anonymous: Some(true),
            protocols: vec!["4".to_string(), "5".to_string()],

            username: None,
            password: None,

            require_certificate: None,
            ca: None,
            server_cert: None,
            server_key: None,
            use_identity_as_username: None,
        }
    }

    pub fn with_listener(mut self, port: u16) -> Self {
        self.listener = port;
        self
    }

    pub fn with_allow_anonymous(mut self, allow_anonymous: bool) -> Self {
        self.allow_anonymous = Some(allow_anonymous);
        self
    }

    pub fn with_protocols(mut self, protocols: Vec<String>) -> Self {
        self.protocols = protocols;
        self
    }

    pub fn with_require_certificate(mut self, require_certificate: bool) -> Self {
        self.require_certificate = Some(require_certificate);
        self
    }

    pub fn with_ca(mut self, ca: impl Into<String>) -> Self {
        self.ca = Some(ca.into());
        self
    }

    pub fn with_server_cert(mut self, cert: impl Into<String>) -> Self {
        self.server_cert = Some(cert.into());
        self
    }

    pub fn with_server_key(mut self, key: impl Into<String>) -> Self {
        self.server_key = Some(key.into());
        self
    }

    pub fn with_use_identity_as_username(mut self, use_identity_as_username: bool) -> Self {
        self.use_identity_as_username = Some(use_identity_as_username);
        self
    }

    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    fn build(&self) -> String {
        let mut config = String::new();

        config.push_str(&format!("listener {}\n", self.listener));

        if let Some(allow_anonymous) = self.allow_anonymous {
            config.push_str(&format!("allow_anonymous {allow_anonymous}\n"));
        } else {
            config.push_str("allow_anonymous true\n");
        }

        if !self.protocols.is_empty() {
            config.push_str(&format!(
                "accept_protocol_versions {}\n",
                self.protocols.join(" ")
            ));
        }

        if let Some(ca) = &self.ca {
            config.push_str("cafile /mosquitto/config/ca.crt\n");
        }
        if let Some(cert) = &self.server_cert {
            config.push_str("certfile /mosquitto/config/server.crt\n");
        }
        if let Some(key) = &self.server_key {
            config.push_str("keyfile /mosquitto/config/server.key\n");
        }

        if let Some(require_certificate) = self.require_certificate {
            config.push_str(&format!("require_certificate {require_certificate}\n"));
        }

        if let Some(use_identity_as_username) = self.use_identity_as_username {
            config.push_str(&format!(
                "use_identity_as_username {use_identity_as_username}\n"
            ));
        }

        if self.username.is_some() {
            config.push_str("password_file /tmp/passwd\n");
        }

        config
    }
}

impl Default for MosquittoConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl From<MosquittoConfig> for String {
    fn from(builder: MosquittoConfig) -> Self {
        builder.build()
    }
}

pub struct GeneratedCerts {
    pub ca: String,
    pub server_cert: String,
    pub server_key: String,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
}

pub struct MosquittoGuard {
    inner: Arc<MosquittoGuardInner>,
    client_event_loop_handle: Option<tokio::task::JoinHandle<()>>,
}

struct MosquittoGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
}

impl MosquittoGuard {
    pub async fn new(config_builder: &MosquittoConfig) -> Result<Self> {
        let container = Self::start_container(config_builder).await?;

        Ok(Self {
            inner: Arc::new(MosquittoGuardInner {
                container: std::sync::Mutex::new(Some(container)),
            }),
            client_event_loop_handle: None,
        })
    }

    async fn start_container(
        config_builder: &MosquittoConfig,
    ) -> Result<ContainerAsync<GenericImage>> {
        use testcontainers::runners::AsyncRunner;
        use testcontainers::ContainerRequest;

        let image = GenericImage::new("eclipse-mosquitto", "2.1.2-alpine");
        let mut request: ContainerRequest<GenericImage> = ContainerRequest::from(image);

        request = request.with_mapped_port(
            config_builder.listener,
            testcontainers::core::ContainerPort::Tcp(config_builder.listener),
        );

        // generate the configuration file.
        let config_data = config_builder.build();

        request = request.with_copy_to(
            "/mosquitto/config/mosquitto.conf",
            config_data.as_bytes().to_vec(),
        );

        if let Some(ca) = &config_builder.ca {
            request = request.with_copy_to("/mosquitto/config/ca.crt", ca.as_bytes().to_vec());
        }
        if let Some(cert) = &config_builder.server_cert {
            request =
                request.with_copy_to("/mosquitto/config/server.crt", cert.as_bytes().to_vec());
        }
        if let Some(key) = &config_builder.server_key {
            request = request.with_copy_to("/mosquitto/config/server.key", key.as_bytes().to_vec());
        }

        if let (Some(username), Some(password)) =
            (&config_builder.username, &config_builder.password)
        {
            let cmd = format!(
                "touch /tmp/passwd && mosquitto_passwd -b /tmp/passwd {username} {password} && mosquitto -c /mosquitto/config/mosquitto.conf"
            );
            request = request.with_cmd(["sh", "-c", &cmd]);
        } else {
            request = request.with_cmd(["mosquitto", "-c", "/mosquitto/config/mosquitto.conf"]);
        }

        let container = request
            .start()
            .await
            .map_err(|e| anyhow!("Failed to start Mosquitto: {e}"))?;

        Ok(container)
    }

    pub async fn get_client(&mut self, mqtt_options: MqttOptions) -> Result<AsyncClient> {
        if (self.client_event_loop_handle.is_some()) {
            return Err(anyhow!(
                "MQTT client already created for this MosquittoGuard"
            ));
        }

        let (client, mut event_loop) = AsyncClient::new(mqtt_options, 10);

        let mut retries = 0;
        loop {
            match event_loop.poll().await {
                Ok(_) => break,
                Err(e) if retries < 5 => {
                    retries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("Failed to connect to MQTT: {e}")),
            }
        }

        self.client_event_loop_handle = Some(tokio::spawn(async move {
            loop {
                if let Err(e) = event_loop.poll().await {
                    debug!("MQTT event loop error: {e}");
                }
            }
        }));

        Ok(client)
    }

    pub async fn get_client_v5(&mut self, mqtt_options: MqttOptionsV5) -> Result<AsyncClientV5> {
        if (self.client_event_loop_handle.is_some()) {
            return Err(anyhow!(
                "MQTT client already created for this MosquittoGuard"
            ));
        }

        let (client, mut event_loop) = AsyncClientV5::new(mqtt_options, 10);

        let mut retries = 0;
        loop {
            match event_loop.poll().await {
                Ok(_) => break,
                Err(e) if retries < 5 => {
                    retries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                Err(e) => return Err(anyhow!("Failed to connect to MQTT: {e}")),
            }
        }

        self.client_event_loop_handle = Some(tokio::spawn(async move {
            loop {
                if let Err(e) = event_loop.poll().await {
                    debug!("MQTT v5 event loop error: {e}");
                }
            }
        }));

        Ok(client)
    }

    pub fn abort_client_event_loop(&mut self) {
        if let Some(handle) = self.client_event_loop_handle.take() {
            handle.abort();
        }
    }

    pub async fn cleanup(self) {
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        if let Some(_container) = container_to_stop {
            match _container.stop().await {
                Ok(_) => debug!("Mosquitto container stopped successfully"),
                Err(e) => debug!("Error stopping Mosquitto container: {e}"),
            }
            drop(_container);
        }
    }
}

pub fn generate_test_certs(
    common_name: &str,
    generate_client_certs: bool,
) -> Result<GeneratedCerts> {
    let ca_key_pair = KeyPair::generate()?;
    let mut ca_params = CertificateParams::default();

    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Drasi Test CA");

    let ca_cert = ca_params.self_signed(&ca_key_pair)?;

    // server certs
    let server_key_pair = KeyPair::generate()?;
    let mut server_params = CertificateParams::default();

    server_params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    server_params.subject_alt_names.push(SanType::DnsName(
        common_name
            .try_into()
            .map_err(|e| anyhow!("invalid DNS SAN common name '{common_name}': {e}"))?,
    ));
    server_params.subject_alt_names.push(SanType::DnsName(
        "localhost"
            .try_into()
            .map_err(|e| anyhow!("invalid DNS SAN localhost: {e}"))?,
    ));

    let server_cert = server_params.signed_by(&server_key_pair, &ca_cert, &ca_key_pair)?;

    // client certs
    let (client_key, client_cert) = if generate_client_certs {
        let client_key_pair = KeyPair::generate()?;
        let mut client_params = CertificateParams::default();

        client_params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        client_params.subject_alt_names.push(SanType::DnsName(
            common_name
                .try_into()
                .map_err(|e| anyhow!("invalid DNS SAN common name '{common_name}': {e}"))?,
        ));
        client_params.subject_alt_names.push(SanType::DnsName(
            "localhost"
                .try_into()
                .map_err(|e| anyhow!("invalid DNS SAN localhost: {e}"))?,
        ));

        let client_cert = client_params.signed_by(&client_key_pair, &ca_cert, &ca_key_pair)?;

        (Some(client_key_pair), Some(client_cert))
    } else {
        (None, None)
    };

    Ok(GeneratedCerts {
        ca: ca_cert.pem(),
        server_cert: server_cert.pem(),
        server_key: server_key_pair.serialize_pem(),
        client_cert: client_cert.map(|c| c.pem()),
        client_key: client_key.map(|k| k.serialize_pem()),
    })
}
