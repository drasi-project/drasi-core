// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
use drasi_computation_contracts::StreamId;
pub use drasi_computation_network_common::config::{backoff, duration, FailurePolicy};
use drasi_computation_network_common::config::{input_stream, normalized_headers, timeout_ms};
pub use drasi_http_common::reaction::config::{
    AdaptiveBatchConfig, HttpOutputTemplates, HttpReactionConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn retries() -> u32 {
    3
}
fn batch_bytes() -> usize {
    16 * 1024 * 1024
}
fn request_bytes() -> usize {
    32 * 1024 * 1024
}

/// Settings and validation for the native HTTP query-output sink.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSinkConfig {
    pub query_id: String,
    pub stream: StreamId,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query_streams: BTreeMap<String, StreamId>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<HttpOutputTemplates>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "drasi_http_common::reaction::config::adaptive_config_serde"
    )]
    pub adaptive: Option<AdaptiveBatchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_endpoint: Option<String>,
    #[serde(default = "batch_bytes")]
    pub max_batch_bytes: usize,
    #[serde(default = "request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub failure_policy: FailurePolicy,
}

impl HttpSinkConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        use serde_json::json;
        ConfigDefinition::try_new([
            Field::new("queryId", String, false, "Query whose result changes this sink accepts; required unless queryStreams is supplied."),
            Field::new("stream", String, false, "Input stream identifier.").default_description("queryId followed by /out."),
            Field::new("queryStreams", Object, false, "Map of query identifiers to connected streams, for a shared sink."),
            Field::new("url", String, false, "Fixed default HTTP destination; supply url or baseUrl."),
            Field::new("baseUrl", String, false, "Base URL for default /changes/{queryId}, template paths and the batch endpoint."),
            Field::new("token", String, false, "Bearer authentication token, preserved in privileged configuration snapshots."),
            Field::new("outputTemplates", Object, false, "Legacy per-query/default operation templates for body, URL, method and headers."),
            Field::new("adaptive", Object, false, "Legacy adaptive batching bounds; requires batchEndpoint. The host retains inputs until delivery completes."),
            Field::new("batchEndpoint", String, false, "Path relative to baseUrl for POST {batch:[...]} delivery."),
            Field::new("maxBatchBytes", Integer, false, "Host collection byte bound; larger individual inputs use direct handling.").default(batch_bytes()).minimum(1).maximum(64 * 1024 * 1024),
            Field::new("maxRequestBytes", Integer, false, "Maximum rendered HTTP request body; adaptive batches split at this boundary.").default(request_bytes()).minimum(1).maximum(32 * 1024 * 1024),
            Field::new("headers", Object, false, "Request headers. JSON content type and the query identity header are supplied automatically.").default(json!({})),
            Field::new("timeoutMs", Integer, false, "HTTP request timeout in milliseconds; must be representable by the system clock.").default(timeout_ms()).minimum(1),
            Field::new("maxRetries", Integer, false, "Additional delivery attempts after the first attempt.").default(retries()).minimum(0).maximum(drasi_computation_network_common::config::MAX_RETRIES as u64),
            Field::new("failurePolicy", String, false, "strict reports delivery failure; skip deliberately discards failed output with an error report.").default(json!(FailurePolicy::default())).allowed([json!(FailurePolicy::Strict), json!(FailurePolicy::Skip)]),
        ]).expect("constant HTTP sink configuration")
    }

    pub fn parse(mut value: Value) -> Result<Self> {
        drasi_computation_network_common::config::validate_output_templates(
            &value,
            &["url", "method", "headers"],
        )?;
        input_stream(&mut value)?;
        let mut config: Self = serde_json::from_value(value)?;
        let url = reqwest::Url::parse(config.base_url.as_deref().unwrap_or(&config.url))?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "unsupported HTTP URL"
        );
        duration(config.timeout_ms, "timeoutMs")?;
        ensure!(
            config.max_retries <= drasi_computation_network_common::config::MAX_RETRIES,
            "maxRetries must not exceed 100"
        );
        config.headers = normalized_headers(&config.headers, &config.query_id)?;
        config
            .headers
            .entry("content-type".into())
            .or_insert("application/json".into());
        ensure!(
            (1..=64 * 1024 * 1024).contains(&config.max_batch_bytes),
            "invalid maxBatchBytes"
        );
        ensure!(
            (1..=32 * 1024 * 1024).contains(&config.max_request_bytes),
            "invalid maxRequestBytes"
        );
        let legacy = config.reaction_config()?;
        let queries = if config.query_streams.is_empty() {
            vec![config.query_id.clone()]
        } else {
            config.query_streams.keys().cloned().collect()
        };
        legacy.validate(&queries, None)?;
        if !config.url.is_empty() {
            let fixed = reqwest::Url::parse(&config.url)?;
            ensure!(
                fixed.username().is_empty()
                    && fixed.password().is_none()
                    && fixed.fragment().is_none(),
                "unsupported HTTP URL"
            );
            drasi_http_common::reaction::config::resolve_http_url(&legacy.base_url, &config.url)?;
        }
        if let Some(adaptive) = &config.adaptive {
            ensure!(
                adaptive.adaptive_max_batch_size <= 65_536
                    && adaptive.adaptive_batch_timeout_ms <= 60_000,
                "adaptive bounds exceed native batching limits"
            );
        }
        Ok(config)
    }

    pub fn reaction_config(&self) -> Result<HttpReactionConfig> {
        let base_url = match &self.base_url {
            Some(base) => base.trim_end_matches('/').to_owned(),
            None => reqwest::Url::parse(&self.url)?
                .origin()
                .ascii_serialization(),
        };
        Ok(HttpReactionConfig {
            base_url,
            token: self.token.clone(),
            timeout_ms: self.timeout_ms,
            output_templates: self.output_templates.clone(),
            adaptive: self.adaptive.clone(),
            batch_endpoint: self.batch_endpoint.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_headers_remain_compatible() -> Result<()> {
        let config = HttpSinkConfig::parse(json!({"queryId":"q","url":"http://localhost/events"}))?;
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "queryId":"q", "stream":"q/out", "url":"http://localhost/events",
                "headers":{"content-type":"application/json","x-query-sequence":"q"},
                "timeoutMs":60000, "maxRetries":3, "failurePolicy":"strict",
                "maxBatchBytes":16777216, "maxRequestBytes":33554432
            })
        );
        Ok(())
    }

    #[test]
    fn validates_http_headers_and_url_in_this_component() {
        for field in [
            json!({"maxRequestBytes":0}),
            json!({"headers":{"x-query-sequence":"other"}}),
            json!({"headers":{"X-Custom":"one","x-custom":"two"}}),
            json!({"headers":{"x-custom":"line\nbreak"}}),
            json!({"url":"ftp://localhost/events"}),
            json!({"maxRetries":101}),
            json!({"outputTemplates":{"routes":{"q":{"adedd":{"template":"bad"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"templat":"bad"}}}}),
        ] {
            let mut value = json!({"queryId":"q","url":"http://localhost/events"});
            value
                .as_object_mut()
                .expect("object")
                .extend(field.as_object().expect("object").clone());
            assert!(HttpSinkConfig::parse(value.clone()).is_err(), "{value}");
        }
    }
}
