// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
pub use drasi_computation_network_common::config::{backoff, duration, FailurePolicy};
use drasi_computation_network_common::config::{input_stream, normalized_headers, timeout_ms};
use drasi_lib::computation::v1::StreamId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn retries() -> u32 {
    3
}

/// Settings and validation for the native HTTP query-output sink.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSinkConfig {
    pub query_id: String,
    pub stream: StreamId,
    pub url: String,
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
    pub fn parse(mut value: Value) -> Result<Self> {
        input_stream(&mut value)?;
        let mut config: Self = serde_json::from_value(value)?;
        let url = reqwest::Url::parse(&config.url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "unsupported HTTP URL"
        );
        duration(config.timeout_ms, "timeoutMs")?;
        ensure!(config.max_retries <= 100, "maxRetries must not exceed 100");
        config.headers = normalized_headers(&config.headers, &config.query_id)?;
        if let Some(value) = config.headers.get("content-type") {
            ensure!(
                value == "application/json",
                "native HTTP output is application/json"
            );
        }
        config
            .headers
            .insert("content-type".into(), "application/json".into());
        Ok(config)
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
                "timeoutMs":60000, "maxRetries":3, "failurePolicy":"strict"
            })
        );
        Ok(())
    }

    #[test]
    fn validates_http_headers_and_url_in_this_component() {
        for field in [
            json!({"headers":{"content-type":"text/plain"}}),
            json!({"headers":{"x-query-sequence":"other"}}),
            json!({"headers":{"X-Custom":"one","x-custom":"two"}}),
            json!({"headers":{"x-custom":"line\nbreak"}}),
            json!({"url":"ftp://localhost/events"}),
            json!({"maxRetries":101}),
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
