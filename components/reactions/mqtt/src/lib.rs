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

mod client;
pub mod config;
pub mod descriptor;
mod mqtt;
mod processor;
mod verifier;
use drasi_lib::identity::IdentityProvider;
use drasi_lib::reactions::common::QueryConfig;
pub use mqtt::MqttReaction;
use std::collections::HashMap;

use crate::config::{
    default_event_channel_capacity, MqttExtension, MqttProtocolVersion, MqttReactionConfig,
    MqttTlsConfig,
};

pub struct MqttReactionBuilder {
    id: String,
    queries: Vec<String>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,

    url: String,
    client_id: Option<String>,
    protocol_version: MqttProtocolVersion,
    routes: HashMap<String, QueryConfig<MqttExtension>>,
    default_template: Option<QueryConfig<MqttExtension>>,
    identity_provider: Option<Box<dyn IdentityProvider>>,
    tls: Option<MqttTlsConfig>,
    event_channel_capacity: usize,
    max_inflight: Option<u16>,
    keep_alive: Option<u64>,
    clean_start: Option<bool>,
    conn_timeout: Option<u64>,
    session_expiry_interval: Option<u32>,
}

impl MqttReactionBuilder {
    // Create a new MQTT reaction builder with given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            priority_queue_capacity: None,
            auto_start: true,

            // Keep defaults aligned with MqttReactionConfig semantics.
            url: "mqtt://localhost:1883".to_string(),
            client_id: None,
            protocol_version: MqttProtocolVersion::default(),
            routes: HashMap::new(),
            default_template: None,
            identity_provider: None,
            tls: None,
            event_channel_capacity: default_event_channel_capacity(),
            max_inflight: None,
            keep_alive: None,
            clean_start: None,
            conn_timeout: None,
            session_expiry_interval: None,
        }
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    pub fn with_protocol_version(mut self, protocol_version: MqttProtocolVersion) -> Self {
        self.protocol_version = protocol_version;
        self
    }

    pub fn with_routes(mut self, routes: HashMap<String, QueryConfig<MqttExtension>>) -> Self {
        self.routes = routes;
        self
    }

    pub fn with_route(
        mut self,
        query_name: impl Into<String>,
        route: QueryConfig<MqttExtension>,
    ) -> Self {
        self.routes.insert(query_name.into(), route);
        self
    }

    pub fn with_routes_extend(
        mut self,
        routes: impl IntoIterator<Item = (String, QueryConfig<MqttExtension>)>,
    ) -> Self {
        self.routes.extend(routes);
        self
    }

    pub fn with_default_template(mut self, default_template: QueryConfig<MqttExtension>) -> Self {
        self.default_template = Some(default_template);
        self
    }

    pub fn with_identity_provider(mut self, identity_provider: Box<dyn IdentityProvider>) -> Self {
        self.identity_provider = Some(identity_provider);
        self
    }

    pub fn with_tls(mut self, tls: MqttTlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }

    pub fn with_event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }

    pub fn with_max_inflight(mut self, max_inflight: u16) -> Self {
        self.max_inflight = Some(max_inflight);
        self
    }

    pub fn with_keep_alive(mut self, keep_alive: u64) -> Self {
        self.keep_alive = Some(keep_alive);
        self
    }

    pub fn with_clean_start(mut self, clean_start: bool) -> Self {
        self.clean_start = Some(clean_start);
        self
    }

    pub fn with_conn_timeout(mut self, conn_timeout: u64) -> Self {
        self.conn_timeout = Some(conn_timeout);
        self
    }

    pub fn with_session_expiry_interval(mut self, session_expiry_interval: u32) -> Self {
        self.session_expiry_interval = Some(session_expiry_interval);
        self
    }

    // Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: MqttReactionConfig) -> Self {
        self.url = config.url;
        self.client_id = config.client_id;
        self.protocol_version = config.protocol_version;
        self.routes = config.routes;
        self.default_template = config.default_template;
        self.identity_provider = config.identity_provider;
        self.tls = config.tls;
        self.event_channel_capacity = config.event_channel_capacity;
        self.max_inflight = config.max_inflight;
        self.keep_alive = config.keep_alive;
        self.clean_start = config.clean_start;
        self.conn_timeout = config.conn_timeout;
        self.session_expiry_interval = config.session_expiry_interval;
        self
    }

    /// Build the MQTT reaction
    pub fn build(self) -> anyhow::Result<MqttReaction> {
        let mut config = MqttReactionConfig {
            url: self.url,
            client_id: self.client_id,
            protocol_version: self.protocol_version,
            routes: self.routes,
            default_template: self.default_template,
            identity_provider: self.identity_provider,
            tls: self.tls,
            event_channel_capacity: self.event_channel_capacity,
            max_inflight: self.max_inflight,
            keep_alive: self.keep_alive,
            clean_start: self.clean_start,
            conn_timeout: self.conn_timeout,
            session_expiry_interval: self.session_expiry_interval,
        };

        config.validate(&self.queries)?;

        MqttReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "mqtt-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::MqttReactionDescriptor],
    bootstrap_descriptors = [],
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MqttQoS;
    use serde_json::json;

    #[test]
    fn yaml_deserializes_reaction_config_with_routes_and_defaults() {
        let yaml = r#"
url: mqtt://broker.example.com:1883
client_id: yaml-client
protocol_version: v3_1_1
event_channel_capacity: 256
max_inflight: 42
keep_alive: 15
clean_start: false
conn_timeout: 9000
session_expiry_interval: 120
default_template:
  added:
    template: "{{after.symbol}}"
    topic: stocks/default/added
routes:
  stocks_query:
    updated:
      template: "{{after.price}}"
      topic: stocks/{{query_name}}/updated
      qos: at_most_once
      retain: true
      empty_payload: true
      message_expiry_interval: 30
"#;

        let config: MqttReactionConfig =
            serde_yaml::from_str(yaml).expect("config should deserialize from yaml");

        assert_eq!(config.url, "mqtt://broker.example.com:1883");
        assert_eq!(config.client_id.as_deref(), Some("yaml-client"));
        assert!(matches!(
            config.protocol_version,
            MqttProtocolVersion::V3_1_1
        ));
        assert_eq!(config.event_channel_capacity, 256);
        assert_eq!(config.max_inflight, Some(42));
        assert_eq!(config.keep_alive, Some(15));
        assert_eq!(config.clean_start, Some(false));
        assert_eq!(config.conn_timeout, Some(9000));
        assert_eq!(config.session_expiry_interval, Some(120));

        let default_added = config
            .default_template
            .as_ref()
            .and_then(|q| q.added.as_ref())
            .expect("default added template should exist");
        assert_eq!(default_added.template, "{{after.symbol}}");
        assert_eq!(default_added.extension.topic, "stocks/default/added");
        assert!(matches!(default_added.extension.qos, MqttQoS::AtLeastOnce));

        let route_updated = config
            .routes
            .get("stocks_query")
            .and_then(|q| q.updated.as_ref())
            .expect("route updated template should exist");
        assert_eq!(route_updated.template, "{{after.price}}");
        assert_eq!(
            route_updated.extension.topic,
            "stocks/{{query_name}}/updated"
        );
        assert!(matches!(route_updated.extension.qos, MqttQoS::AtMostOnce));
        assert!(route_updated.extension.retain);
        assert!(route_updated.extension.empty_payload);
        assert_eq!(route_updated.extension.message_expiry_interval, Some(30));
    }

    #[test]
    fn builder_with_config_maps_yaml_values_into_reaction_properties() {
        let yaml = r#"
url: mqtt://localhost:1883
client_id: mapped-client
protocol_version: v3_1_1
event_channel_capacity: 321
max_inflight: 8
keep_alive: 20
clean_start: true
conn_timeout: 7000
session_expiry_interval: 66
routes:
  stocks_query:
    added:
      template: "{{after.symbol}}"
      topic: stocks/{{query_name}}/added
"#;

        let config: MqttReactionConfig =
            serde_yaml::from_str(yaml).expect("config should deserialize from yaml");

        let reaction = MqttReactionBuilder::new("yaml-rx")
            .with_query("stocks_query")
            .with_config(config)
            .build()
            .expect("reaction should build from yaml config");

        let props = drasi_lib::Reaction::properties(&reaction);
        assert_eq!(props.get("client_id"), Some(&json!("mapped-client")));
        assert_eq!(props.get("url"), Some(&json!("mqtt://localhost:1883")));
        assert_eq!(props.get("protocol_version"), Some(&json!("v3_1_1")));
        assert_eq!(props.get("event_channel_capacity"), Some(&json!(321)));
        assert_eq!(props.get("max_inflight"), Some(&json!(8)));
        assert_eq!(props.get("keep_alive"), Some(&json!(20)));
        assert_eq!(props.get("clean_start"), Some(&json!(true)));
        assert_eq!(props.get("conn_timeout"), Some(&json!(7000)));
        assert_eq!(props.get("session_expiry_interval"), Some(&json!(66)));
        assert_eq!(props.get("routes_count"), Some(&json!(1)));
    }

    #[test]
    fn yaml_deserialization_rejects_unknown_fields() {
        let yaml = r#"
url: mqtt://localhost:1883
unknown_field: true
"#;

        let err = serde_yaml::from_str::<MqttReactionConfig>(yaml)
            .err()
            .expect("unknown fields must be rejected");
        assert!(err.to_string().contains("unknown field"));
    }
}
