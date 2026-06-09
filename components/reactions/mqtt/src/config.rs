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

//! Configuration types for MQTT reaction.
//!
//! This module contains configuration types for MQTT reaction and shared types.
use drasi_lib::reactions::common::TemplateRouting;
pub use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_lib::identity::IdentityProvider;
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_event_channel_capacity() -> usize {
    100
}


#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum MqttQoS {
    AtMostOnce,  // QoS 0
    #[default] 
    AtLeastOnce, // QoS 1
}

fn default_qos() -> MqttQoS {
    MqttQoS::AtLeastOnce
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MqttExtension {
    /// Target MQTT topic. Handlebars template, rendered against the same context as `template`
    /// (`after`, `before`, `query_name`, `operation`, `timestamp`).
    pub topic: String,

    /// QoS level. Default: `AtLeastOnce` (1). Only `AtMostOnce` (0) and `AtLeastOnce` (1) are valid;
    /// the enum has no `ExactlyOnce` variant. See "QoS limits" below.
    #[serde(default = "default_qos")]
    pub qos: MqttQoS,

    /// Retain flag. Default: false.
    #[serde(default)]
    pub retain: bool,

    /// Publish a zero-byte payload regardless of `template`. Default: false.
    /// Pair with `retain: true` on a `deleted` config to clear a retained state-topic message.
    /// Disambiguates from `template: ""`, which means "use the raw-JSON default".
    #[serde(default)]
    pub empty_payload: bool,

    /// MQTT v5 message expiry interval in seconds. Default: None (broker default, never expires).
    /// Silently omitted on v3.1.1 connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_expiry_interval: Option<u32>,


    /// MQTT Topic Slashes count
    #[serde(skip)]
    pub slashes_count: usize,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MqttTlsConfig {
    /// Custom CA bundle (PEM). `None` (default) uses the system CA store, which is the right
    /// answer for HiveMQ Cloud and any public-CA broker.
    #[serde(default)]
    pub ca: Option<Vec<u8>>,

    /// Optional ALPN protocol list (e.g. `vec![b"mqtt".to_vec()]` for HiveMQ Cloud).
    /// Without it, some HiveMQ Cloud handshakes silently fail.
    #[serde(default)]
    pub alpn: Option<Vec<Vec<u8>>>,

    /// Reserved for v2: mTLS client cert + key (PEM). v1 rejects `Some(...)` at startup
    /// with a "deferred to v2" error; the field is in the struct so the v2 addition is non-breaking.
    #[serde(default)]
    pub client_auth: Option<(Vec<u8>, Vec<u8>)>,

    /// Dev-only: skip broker certificate verification. Default `false`. When `true`,
    /// the reaction emits a loud WARN log on every connect. For local Mosquitto without
    /// proper certs; never use in production.
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MqttReactionConfig {
    /// Broker URL. Examples:
    ///   `mqtt://broker.example.com:1883`
    ///   `mqtts://broker.example.com:8883`
    ///   `ws://broker.example.com:80/mqtt`
    ///   `wss://broker.example.com:443/mqtt`
    /// Scheme selects transport: `mqtt` = plain TCP, `mqtts` = TLS over TCP,
    /// `ws` = WebSocket, `wss` = TLS over WebSocket. Default ports per scheme: 1883 / 8883 / 80 / 443.
    /// Parsed at startup; an unsupported scheme is a startup error.
    pub url: String,

    /// Optional client ID. Defaults to `drasi-mqtt-{reaction_id}` for deterministic
    /// session identity across restarts. Required by AWS IoT and Azure IoT Hub;
    /// required for resuming persistent sessions when `clean_start: false`.
    /// Do NOT default to a random UUID; that orphans broker sessions on every restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// MQTT protocol version. Default: V5. Selected once at startup; no auto-fallback.
    #[serde(default)]
    pub protocol_version: MqttProtocolVersion,

    /// Per-query routing: query_id -> per-operation template config.
    /// Matches the convention in log, SSE, HTTP, and storedproc reactions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig<MqttExtension>>,

    /// Default template fallback when a query has no entry in `routes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig<MqttExtension>>,

    /// Identity provider for authentication. Wired via `with_identity_provider(...)` builder.
    /// In v1, supply a `PasswordIdentityProvider` (from `lib/src/identity/password.rs`)
    /// for username/password brokers. Token and certificate providers are deferred to v2.
    #[serde(skip)]
    pub identity_provider: Option<Box<dyn IdentityProvider>>,

    /// TLS tuning. Required when the URL scheme is `mqtts` or `wss`; ignored otherwise.
    /// Startup validation: scheme says TLS but `tls: None` is an error;
    /// scheme is plain but `tls: Some(...)` is also an error (defensive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<MqttTlsConfig>,

    /// Capacity of the rumqttc internal channel between AsyncClient and EventLoop.
    /// Default: 100. Sized for sustained throughput; see "Backpressure".
    #[serde(default = "default_event_channel_capacity")]
    pub event_channel_capacity: usize,

    /// Maximum outgoing inflight QoS 1 messages. Default: rumqttc default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight: Option<u16>,

    /// Keep-alive interval in seconds (PingReq cadence). Default: 60.
    /// Lower for IoT-over-NAT scenarios where idle connections get reaped.
    /// Should be greater than or equal to 5 seconds based on rumqttc implementation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<u64>,

    /// Clean session start. Default: true.
    /// Set false plus `client_id` to resume a persistent broker-side session;
    /// note that v1 has no client-side in-flight persistence (see "Shutdown semantics").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean_start: Option<bool>,

    /// MQTT v5 connection timeout in milliseconds. Default: rumqttc default (5000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn_timeout: Option<u64>,

    /// MQTT v5 session expiry interval. Meaningful only with `clean_start: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_expiry_interval: Option<u32>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MqttProtocolVersion {
    #[default]
    V5,
    V3_1_1,
}

impl TemplateRouting<MqttExtension> for MqttReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig<MqttExtension>> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig<MqttExtension>> {
        self.default_template.as_ref()
    }
}

impl MqttReactionConfig {
    pub fn validate(
        &self,
        queries: &Vec<String>
    ) -> anyhow::Result<()>
    {

        // initial validation for values
        if self.event_channel_capacity == 0 {
            return Err(anyhow::anyhow!("Event channel capacity must be greater than 0"));
        }

        if self.max_inflight.map_or(false, |m| m == 0) {
            return Err(anyhow::anyhow!("Max inflight messages must be greater than 0"));
        }

        if self.keep_alive.map_or(false, |k| k < 5) {
            return Err(anyhow::anyhow!("Keep-alive interval must be at least 5 seconds"));
        }

        if self.conn_timeout .map_or(false, |t| t == 0) {
            return Err(anyhow::anyhow!("Connection timeout must be greater than 0"));
        }

        if self.session_expiry_interval.map_or(false, |s| s == 0) {
            return Err(anyhow::anyhow!("Session expiry interval must be greater than 0"));
        }

        // validate the TLS config
        self.validate_tls_config()?;

        // validate the type of the URL and its scheme
        self.validate_url_type()?;

        // validate the routes and default template
        self.validate_routes(queries)?;

        // validate the default cquery config
        if let Some(default_template) = self.default_template.as_ref() {
            self.validate_route(default_template)?;
        }


        Ok(())
    }
    
    fn validate_tls_config(
        &self
    ) -> anyhow::Result<()> {
        if let Some(tls_config) = self.tls.as_ref() {
            if tls_config.client_auth.is_some() {
                return Err(anyhow::anyhow!(
                    "Client authentication is not supported in v1 and it is deferred to v2; the 'client_auth' field must be None"
                ));
            }
        }
        Ok(())
    }

    fn validate_url_type(
        &self
    ) -> anyhow::Result<()>
    {
        let url = url::Url::parse(&self.url)
            .map_err(|e| anyhow::anyhow!("Invalid MQTT broker URL '{}': {}", self.url, e))?;

        // get the schema
        let schema = url.scheme();

        // validate the schema values
        let valid_schemes = ["mqtt", "mqtts", "ws", "wss"];
        if !valid_schemes.contains(&schema) {
            return Err(anyhow::anyhow!(
                "Unsupported URL scheme '{}'. Valid schemes are: {:?}",
                schema, valid_schemes
            ));
        }

        // validate TLS config based on the schema
        match schema {
            "mqtts" | "wss" => {
                if self.tls.is_none() {
                    return Err(anyhow::anyhow!(
                        "TLS configuration is required for URL scheme '{}'", schema
                    ));
                }
            }
            "mqtt" | "ws" => {
                if self.tls.is_some() {
                    return Err(anyhow::anyhow!(
                        "TLS configuration should be None for URL scheme '{}'", schema
                    ));
                }
            }
            _ => unreachable!(), // already validated above
        }
        Ok(())
    }

    fn validate_routes(
        &self,
        queries: &Vec<String>
    ) -> anyhow::Result<()> {

        let unique_queries = queries.iter().collect::<std::collections::HashSet<_>>();
        // check that all routes reference valid queries and validate each route config
        let mut subscribed_queries_ctr = 0;
        for (query_id, query_config) in &self.routes {
            if !unique_queries.contains(query_id) {
                return Err(
                    anyhow::anyhow!("Route defined for query '{}' which is not in the list of valid queries", query_id)
                );
            } else {
                self.validate_route(query_config)?;
                subscribed_queries_ctr += 1;
            }
        }
        Ok(())
    }

    fn validate_route(
        &self,
        route_config: &QueryConfig<MqttExtension>
    ) -> anyhow::Result<()> {
        
        if let Some(added) = route_config.added.as_ref() {
            self.validate_template(&added.template)?;
            self.validate_topic(&added.extension.topic)?;
        }

        if let Some(updated) = route_config.updated.as_ref() {
            self.validate_template(&updated.template)?;
            self.validate_topic(&updated.extension.topic)?;
        }

        if let Some(deleted) = route_config.deleted.as_ref() {
            self.validate_template(&deleted.template)?;
            self.validate_topic(&deleted.extension.topic)?;
        }

        Ok(())
    }

    fn validate_template(
        &self,
        template: &str
    ) -> anyhow::Result<()> {
        
        Ok(())
    }

    fn validate_topic(
        &self,
        topic: &str
    ) -> anyhow::Result<()> {

        if topic.len() == 0 {
            return Err(anyhow::anyhow!("Topic cannot be empty"));
        }
        
        let topic_bytes = topic.as_bytes();
        let topic_len = topic_bytes.len();

        for i in 0..topic_len {
            let b = topic_bytes[i];
            if b == 0 {
                return Err(anyhow::anyhow!("Topic cannot contain null character"));
            }

            if b == b'+' || b == b'#' {
                return Err(anyhow::anyhow!("Topic cannot contain wildcard characters '+' or '#'"));
            }

            if i != 0 && b == b'/' && topic_bytes[i - 1] == b'/' {
                return Err(anyhow::anyhow!("Topic cannot contain empty levels (consecutive '/')"));
            }
        }
        Ok(())
    }

    
}