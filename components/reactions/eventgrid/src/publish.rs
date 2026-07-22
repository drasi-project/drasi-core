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
/// Maximum number of bytes of an error-response body reflected into logs.
const MAX_DETAIL_LEN: usize = 512;

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

    // Serialise manually and send via `.body()`. `RequestBuilder::json()` would
    // overwrite the explicit `Content-Type` header (e.g.
    // `application/cloudevents-batch+json`) with `application/json`, causing
    // Event Grid to reject or misinterpret the batch.
    let body_bytes = serde_json::to_vec(body).context("failed to serialise event batch")?;

    for attempt in 1..=MAX_DELIVERY_ATTEMPTS {
        let mut request = client.post(endpoint).header("Content-Type", content_type);
        request = match auth {
            Auth::AccessKey(key) => request.header("aeg-sas-key", key),
            Auth::Bearer(token) => request.bearer_auth(token),
        };

        match request.body(body_bytes.clone()).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(());
                }

                let retriable = status.is_server_error()
                    || status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::REQUEST_TIMEOUT;
                let detail = sanitize_detail(&resp.text().await.unwrap_or_default());
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
                if attempt < MAX_DELIVERY_ATTEMPTS {
                    warn!(
                        "[{reaction_name}] Publish attempt {attempt}/{MAX_DELIVERY_ATTEMPTS} failed ({status}), retrying"
                    );
                }
                last_err = Some(err);
            }
            Err(e) => {
                if attempt < MAX_DELIVERY_ATTEMPTS {
                    warn!(
                        "[{reaction_name}] Publish attempt {attempt}/{MAX_DELIVERY_ATTEMPTS} network error: {e}, retrying"
                    );
                }
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

/// Sanitize an error-response body before reflecting it into logs/errors.
///
/// The endpoint may be attacker-controlled (e.g. via SSRF or a compromised
/// topic), so its body is untrusted. Truncate to [`MAX_DETAIL_LEN`] bytes at a
/// UTF-8 char boundary to bound memory, and replace control characters (ANSI
/// escapes, newlines, etc.) with spaces to prevent log injection.
fn sanitize_detail(detail: &str) -> String {
    // Truncate on a char boundary so slicing never panics on multibyte input.
    let mut end = detail.len().min(MAX_DETAIL_LEN);
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &detail[..end];

    let mut out: String = truncated
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if end < detail.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod publish_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_body() -> Value {
        json!([{ "id": "1" }])
    }

    #[test]
    fn sanitize_detail_strips_control_chars() {
        let input = "bad\u{1b}[31mred\u{1b}[0m\nline";
        let out = sanitize_detail(input);
        assert!(!out.contains('\u{1b}'));
        assert!(!out.contains('\n'));
        assert_eq!(out, "bad [31mred [0m line");
    }

    #[test]
    fn sanitize_detail_truncates_on_char_boundary() {
        // Multibyte chars straddling the byte limit must not panic and the
        // output must stay valid UTF-8.
        let input = "é".repeat(MAX_DELIVERY_ATTEMPTS + MAX_DETAIL_LEN);
        let out = sanitize_detail(&input);
        assert!(out.ends_with('…'));
        // Truncated slice is <= the byte bound (plus the ellipsis).
        assert!(out.len() <= MAX_DETAIL_LEN + '…'.len_utf8());
    }

    #[test]
    fn sanitize_detail_passes_through_short_clean_text() {
        assert_eq!(sanitize_detail("not found"), "not found");
        assert_eq!(sanitize_detail(""), "");
    }

    #[tokio::test]
    async fn publish_retries_5xx_then_succeeds() {
        let server = MockServer::start().await;
        // First response 503 (retriable), then 200 on the retry.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client(5_000).unwrap();
        let result = publish(
            &client,
            &format!("{}/api/events", server.uri()),
            &Auth::AccessKey("k".into()),
            "application/json",
            &test_body(),
            "test",
        )
        .await;
        assert!(
            result.is_ok(),
            "expected success after one retry: {result:?}"
        );
    }

    #[tokio::test]
    async fn publish_exhausts_retries_on_persistent_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            // 429 is retriable, so every attempt hits the server.
            .expect(MAX_DELIVERY_ATTEMPTS as u64)
            .mount(&server)
            .await;

        let client = build_client(5_000).unwrap();
        let result = publish(
            &client,
            &format!("{}/api/events", server.uri()),
            &Auth::AccessKey("k".into()),
            "application/json",
            &test_body(),
            "test",
        )
        .await;
        assert!(result.is_err(), "expected error after exhausting retries");
    }

    #[tokio::test]
    async fn publish_returns_err_on_non_retriable_4xx() {
        let server = MockServer::start().await;
        // 401 is not retriable: exactly one attempt, then a hard error.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client(5_000).unwrap();
        let result = publish(
            &client,
            &format!("{}/api/events", server.uri()),
            &Auth::AccessKey("k".into()),
            "application/json",
            &test_body(),
            "test",
        )
        .await;
        assert!(result.is_err(), "expected non-retriable failure");
    }

    #[tokio::test]
    async fn publish_returns_err_on_network_error() {
        // Nothing is listening on this port: every attempt is a connection error
        // (the retriable network branch), and the call ends in an error.
        let client = build_client(1_000).unwrap();
        let result = publish(
            &client,
            "http://127.0.0.1:1/api/events",
            &Auth::AccessKey("k".into()),
            "application/json",
            &test_body(),
            "test",
        )
        .await;
        assert!(result.is_err(), "expected network error to surface");
    }
}
