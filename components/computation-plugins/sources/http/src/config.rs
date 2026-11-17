// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration for `drasi.network/http-source`.

use anyhow::{ensure, Result};
use drasi_computation_contracts::StreamId;
use drasi_computation_network_common::config::{
    self as shared, IngressConfig, SecretString, SecuritySettings,
};
pub use drasi_http_common::webhook::config::WebhookConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_BATCH_EVENTS: usize = 1_048_576;

fn port() -> u16 {
    9000
}
fn message_bytes() -> usize {
    2 * 1024 * 1024
}
fn batch_events() -> usize {
    1024
}
fn header_read_timeout_ms() -> u64 {
    10_000
}

/// Settings for one native HTTP source.
///
/// Secret fields (`authToken`, `authTokenSecondary`, `tlsPrivateKeyPem`) are
/// supplied as host secret references and are never printed by `Debug`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSourceConfig {
    /// Output stream identifier.
    pub stream: StreamId,
    /// Literal IP address to listen on.
    #[serde(default = "shared::host")]
    pub host: String,
    /// TCP port; zero requests an available port.
    #[serde(default = "port")]
    pub port: u16,
    /// Request-body and admission deadline in milliseconds.
    #[serde(default = "shared::timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum simultaneous admitting requests; also the volatile queue bound.
    #[serde(default = "shared::ingress_capacity")]
    pub ingress_capacity: usize,
    /// Maximum request-body size in bytes.
    #[serde(default = "message_bytes")]
    pub max_message_bytes: usize,
    /// Maximum events in one batch request.
    #[serde(default = "batch_events")]
    pub max_batch_events: usize,
    /// Maximum simultaneously open connections.
    #[serde(default = "shared::max_connections")]
    pub max_connections: usize,
    /// Deadline for a client to send complete request headers.
    #[serde(default = "header_read_timeout_ms")]
    pub header_read_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_secondary: Option<SecretString>,
    #[serde(default)]
    pub allow_unauthenticated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_burst: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_private_key_pem: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_ca_pem: Option<String>,
    #[serde(default)]
    pub adaptive_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_batch_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_batch_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_window_secs: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "drasi_http_common::webhook::dto::optional"
    )]
    pub webhooks: Option<WebhookConfig>,
}

impl HttpSourceConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        let mut fields = shared::ingress_options(
            port(),
            message_bytes(),
            "Maximum HTTP request-body size in bytes.",
        );
        fields.extend([
            Field::new("maxBatchEvents", Integer, false, "Maximum events in one batch request.").default(batch_events()).minimum(1).maximum(MAX_BATCH_EVENTS as u64),
            Field::new("headerReadTimeoutMs", Integer, false, "Deadline in milliseconds for a client to send complete request headers; bounds slow clients and graceful shutdown.").default(header_read_timeout_ms()).minimum(1).maximum(600_000),
            Field::new("adaptiveEnabled", Boolean, false, "Enable bounded prefetch in volatile mode using throughput tiers. Each event remains a separate graph envelope.").default(false),
            Field::new("adaptiveMaxBatchSize", Integer, false, "Maximum prefetched events, no greater than ingressCapacity.").default_description("The smaller of 1000 and ingressCapacity.").minimum(1).maximum(shared::MAX_INGRESS_CAPACITY as u64),
            Field::new("adaptiveMinBatchSize", Integer, false, "Minimum adaptive collection target, no greater than adaptiveMaxBatchSize.").default_description("The smaller of 10 and adaptiveMaxBatchSize.").minimum(1).maximum(shared::MAX_INGRESS_CAPACITY as u64),
            Field::new("adaptiveMaxWaitMs", Integer, false, "Maximum collection wait in milliseconds.").default_description("100").minimum(1).maximum(60_000),
            Field::new("adaptiveMinWaitMs", Integer, false, "Minimum collection wait in milliseconds.").default_description("1").minimum(1).maximum(60_000),
            Field::new("adaptiveWindowSecs", Integer, false, "Throughput observation window in seconds.").default_description("5").minimum(1).maximum(3600),
            Field::new("webhooks", Object, false, "Custom webhook routes with authentication, JSON/XML/YAML/text parsing, conditions, mapping, timestamps and CORS. When present, replaces the standard event routes. See CONFIGURATION.md for the nested schema."),
        ]);
        ConfigDefinition::try_new(fields)
            .unwrap_or_else(|error| unreachable!("constant HTTP source configuration: {error}"))
    }

    pub fn from_value(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.ingress()?.validate()?;
        self.adaptive_settings()?;
        ensure!(
            (1..=MAX_BATCH_EVENTS).contains(&self.max_batch_events),
            "maxBatchEvents must be in 1..=1048576"
        );
        ensure!(
            (1..=600_000).contains(&self.header_read_timeout_ms),
            "headerReadTimeoutMs must be in 1..=600000"
        );
        if let Some(webhooks) = &self.webhooks {
            webhooks.validate()?;
            ensure!(
                webhooks
                    .routes
                    .iter()
                    .all(|route| route.mappings.len() <= self.max_batch_events),
                "webhook mapping count exceeds maxBatchEvents"
            );
            for route in &webhooks.routes {
                ensure!(
                    route.path != "/health" && !route.path.starts_with("/sources/"),
                    "webhook route {} collides with a built-in route (/health or /sources/...)",
                    route.path
                );
            }
        }
        Ok(())
    }

    /// Whether custom webhook routes replace the standard event routes.
    pub fn webhook_mode(&self) -> bool {
        self.webhooks.is_some()
    }

    pub fn ingress(&self) -> Result<IngressConfig> {
        let security = SecuritySettings {
            auth_token: self.auth_token.as_ref(),
            auth_token_secondary: self.auth_token_secondary.as_ref(),
            allow_unauthenticated: self.allow_unauthenticated,
            rate_limit_per_second: self.rate_limit_per_second,
            rate_limit_burst: self.rate_limit_burst,
            tls_certificate_pem: self.tls_certificate_pem.as_deref(),
            tls_private_key_pem: self.tls_private_key_pem.as_ref(),
            tls_client_ca_pem: self.tls_client_ca_pem.as_deref(),
        }
        .build()?;
        Ok(IngressConfig {
            stream: self.stream.clone(),
            host: self.host.clone(),
            port: self.port,
            timeout_ms: self.timeout_ms,
            ingress_capacity: self.ingress_capacity,
            max_message_bytes: self.max_message_bytes,
            max_connections: self.max_connections,
            security,
        })
    }

    /// Refuses unauthenticated exposure on a non-loopback address. `durable`
    /// states whether the source has a durable admission binding, in which
    /// case the token-protected admission routes also admit data.
    pub fn validate_exposure(&self, durable: bool) -> Result<()> {
        let ingress = self.ingress()?;
        ingress.validate_exposure(!self.webhook_mode() || durable)?;
        if let Some(webhooks) = &self.webhooks {
            if !ingress.ip()?.is_loopback() && !self.allow_unauthenticated {
                for route in &webhooks.routes {
                    ensure!(
                        route
                            .auth
                            .as_ref()
                            .is_some_and(|auth| auth.signature.is_some() || auth.bearer.is_some()),
                        "webhook route {} has no authentication; a non-loopback listener requires route authentication or allowUnauthenticated: true",
                        route.path
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn adaptive_settings(
        &self,
    ) -> Result<Option<drasi_http_common::adaptive_batcher::AdaptiveBatchConfig>> {
        if !self.adaptive_enabled {
            ensure!(
                self.adaptive_max_batch_size.is_none()
                    && self.adaptive_min_batch_size.is_none()
                    && self.adaptive_max_wait_ms.is_none()
                    && self.adaptive_min_wait_ms.is_none()
                    && self.adaptive_window_secs.is_none(),
                "adaptive settings require adaptiveEnabled"
            );
            return Ok(None);
        }
        let mut config = drasi_http_common::adaptive_batcher::AdaptiveBatchConfig::default();
        config.max_batch_size = self
            .adaptive_max_batch_size
            .unwrap_or(1000.min(self.ingress_capacity));
        config.min_batch_size = self
            .adaptive_min_batch_size
            .unwrap_or(10.min(config.max_batch_size));
        config.max_wait_time = shared::duration(
            self.adaptive_max_wait_ms.unwrap_or(100),
            "adaptiveMaxWaitMs",
        )?;
        config.min_wait_time =
            shared::duration(self.adaptive_min_wait_ms.unwrap_or(1), "adaptiveMinWaitMs")?;
        let window = self.adaptive_window_secs.unwrap_or(5);
        ensure!(
            (1..=3600).contains(&window),
            "adaptiveWindowSecs must be in 1..=3600"
        );
        config.throughput_window = std::time::Duration::from_secs(window);
        ensure!(
            config.min_batch_size > 0
                && config.min_batch_size <= config.max_batch_size
                && config.max_batch_size <= self.ingress_capacity,
            "adaptive batch bounds exceed ingressCapacity or are inverted"
        );
        ensure!(
            config.min_wait_time <= config.max_wait_time
                && config.max_wait_time <= std::time::Duration::from_secs(60),
            "invalid adaptive wait bounds"
        );
        Ok(Some(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_serialize_explicitly_and_bind_loopback() -> Result<()> {
        let config = HttpSourceConfig::from_value(json!({"stream":"events/out"}))?;
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "stream":"events/out", "host":"127.0.0.1",
                "port":9000, "timeoutMs":60000, "ingressCapacity":1024,
                "maxMessageBytes":2097152, "maxBatchEvents":1024, "maxConnections":1024,
                "headerReadTimeoutMs":10000, "allowUnauthenticated":false,
                "adaptiveEnabled":false
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_settings() {
        for field in [
            json!({"adaptiveEnabled":true,"adaptiveMinBatchSize":0}),
            json!({"maxBatchEvents":0}),
            json!({"maxBatchEvents":null}),
            json!({"maxMessageBytes":0}),
            json!({"ingressCapacity":0}),
            json!({"maxConnections":0}),
            json!({"headerReadTimeoutMs":0}),
            json!({"timeoutMs":0}),
            json!({"host":"localhost"}),
            json!({"authToken":"short"}),
            json!({"authTokenSecondary":"0123456789abcdef"}),
            json!({"rateLimitBurst":4}),
            json!({"tlsCertificatePem":"x"}),
            json!({"sourceId":"nested/source"}),
            json!({"unknown":true}),
        ] {
            let mut value = json!({"stream":"events/out"});
            value
                .as_object_mut()
                .expect("object")
                .extend(field.as_object().expect("object").clone());
            assert!(
                HttpSourceConfig::from_value(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn non_loopback_exposure_requires_authentication() -> Result<()> {
        let open = HttpSourceConfig::from_value(json!({"stream":"s","host":"0.0.0.0"}))?;
        assert!(open.validate_exposure(false).is_err());
        let token = HttpSourceConfig::from_value(
            json!({"stream":"s","host":"0.0.0.0","authToken":"0123456789abcdef"}),
        )?;
        token.validate_exposure(true)?;
        let opted_in = HttpSourceConfig::from_value(
            json!({"stream":"s","host":"0.0.0.0","allowUnauthenticated":true}),
        )?;
        opted_in.validate_exposure(true)?;
        let webhook = |auth: Value| {
            HttpSourceConfig::from_value(
                json!({"stream":"s","host":"0.0.0.0","webhooks":{"routes":[{
                    "path":"/hook","methods":["POST"],"auth":auth,
                    "mappings":[{"operation":"insert","elementType":"node","template":{"id":"{{payload.id}}","labels":["A"]}}]
                }]}}),
            )
        };
        // Volatile webhook mode relies on route authentication, not the token.
        assert!(webhook(Value::Null)?.validate_exposure(false).is_err());
        webhook(json!({"bearer":{"tokenEnv":"HOOK_TOKEN"}}))?.validate_exposure(false)?;
        // Durable webhook mode also exposes the admission routes.
        assert!(webhook(json!({"bearer":{"tokenEnv":"HOOK_TOKEN"}}))?
            .validate_exposure(true)
            .is_err());
        Ok(())
    }

    #[test]
    fn webhook_routes_cannot_shadow_built_in_routes() {
        for path in ["/health", "/sources/x/events"] {
            let value = json!({"stream":"s","webhooks":{"routes":[{
                "path":path,"methods":["POST"],
                "mappings":[{"operation":"insert","elementType":"node","template":{"id":"{{payload.id}}","labels":["A"]}}]
            }]}});
            assert!(HttpSourceConfig::from_value(value).is_err(), "{path}");
        }
    }
}
