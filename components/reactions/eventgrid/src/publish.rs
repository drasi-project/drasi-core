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

//! HTTP publishing to an Event Grid custom-topic endpoint.
//!
//! Event Grid custom-topic publishing is a plain HTTP `POST` of a JSON array of
//! events. Authentication is either the topic access key (`aeg-sas-key` header)
//! or an AAD bearer token (`Authorization: Bearer …`).

use std::time::Duration;

use anyhow::{Context, Result};
use log::warn;
use reqwest::{Client, StatusCode};
use serde_json::Value;

/// Maximum number of delivery attempts for a single batch.
const MAX_DELIVERY_ATTEMPTS: usize = 3;
/// Initial backoff before the first retry; doubles each attempt.
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// Authentication mode for a publish call.
#[derive(Clone)]
pub enum Auth {
    /// Topic access key sent via the `aeg-sas-key` header.
    AccessKey(String),
    /// AAD bearer token sent via the `Authorization` header.
    Bearer(String),
}

/// Build a pooled reqwest client with the configured request timeout.
pub fn build_client(timeout_ms: u64) -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(10)
        .build()
        .context("failed to build Event Grid HTTP client")
}

/// Publish a batch (`body`, a JSON array of events) to `endpoint`.
///
/// Transient failures (network errors and 5xx responses) are retried up to
/// [`MAX_DELIVERY_ATTEMPTS`] with exponential backoff. Non-transient failures
/// (4xx) fail immediately.
pub async fn publish(
    client: &Client,
    endpoint: &str,
    auth: &Auth,
    content_type: &str,
    body: &Value,
    reaction_name: &str,
) -> Result<()> {
    let mut backoff = INITIAL_RETRY_BACKOFF;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_DELIVERY_ATTEMPTS {
        let mut request = client.post(endpoint).header("Content-Type", content_type);
        request = match auth {
            Auth::AccessKey(key) => request.header("aeg-sas-key", key),
            Auth::Bearer(token) => request.bearer_auth(token),
        };

        match request.json(body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(());
                }

                let retriable = status.is_server_error()
                    || status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT;
                let detail = resp.text().await.unwrap_or_default();
                let err = anyhow::anyhow!(
                    "Event Grid returned {status}{}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                );

                if !retriable {
                    return Err(err);
                }
                warn!(
                    "[{reaction_name}] Publish attempt {attempt}/{MAX_DELIVERY_ATTEMPTS} failed ({status}), retrying"
                );
                last_err = Some(err);
            }
            Err(e) => {
                warn!(
                    "[{reaction_name}] Publish attempt {attempt}/{MAX_DELIVERY_ATTEMPTS} network error: {e}, retrying"
                );
                last_err = Some(anyhow::Error::new(e).context("Event Grid request failed"));
            }
        }

        if attempt < MAX_DELIVERY_ATTEMPTS {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Event Grid publish failed")))
}
