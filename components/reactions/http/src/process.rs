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

//! Per-result HTTP delivery used by both standard and adaptive runtime loops.

use anyhow::{anyhow, Result};
use handlebars::Handlebars;
use log::{debug, error, warn};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method, StatusCode,
};
use serde_json::{Map, Value};
use std::time::Duration;

use crate::config::{
    parse_http_method, resolve_http_url, HttpCallSpec, HttpReactionConfig, TemplateRouting,
};
use crate::output::{DefaultChangeNotification, Operation};

const MAX_DELIVERY_ATTEMPTS: usize = 3;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// Maximum length of a downstream-supplied string (response body, rendered URL)
/// allowed into a log line.
const MAX_LOG_FIELD_LEN: usize = 512;

/// Bound and sanitize a downstream-controlled string before logging it: cap the
/// length and strip control characters (notably CR/LF) so a hostile or
/// misconfigured server cannot bloat logs or inject fake log lines.
fn truncate_for_log(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(MAX_LOG_FIELD_LEN)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().count() > MAX_LOG_FIELD_LEN {
        out.push('…');
    }
    out
}

/// Outcome of attempting to deliver a single result to its HTTP target.
///
/// `Err` is reserved for **transient/sustained** failures that warrant the
/// reaction's recovery policy: 5xx and other retryable statuses after the retry
/// budget is exhausted, connection errors, and **auth/permission rejections**
/// (401/403/407 — recoverable by refreshing a credential, so the event must
/// replay rather than be lost). Only genuinely **permanent "poison"** conditions
/// (most 4xx — 400/404/405/422/…, SSRF-blocked or unresolvable URLs, invalid
/// method/auth-token config) resolve to `Ok(Dropped)`: they can never succeed on
/// replay, so the event is dropped and the checkpoint advances past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// The downstream accepted the request.
    Delivered,
    /// The event was permanently undeliverable and dropped.
    Dropped,
}

/// Build a [`Handlebars`] registry pre-loaded with the `json` helper used
/// across all HTTP templates.
pub(crate) fn build_handlebars() -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &handlebars::Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             out: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                if let Some(value) = h.param(0) {
                    let json_str = serde_json::to_string(&value.value())
                        .unwrap_or_else(|_| "null".to_string());
                    out.write(&json_str)?;
                }
                Ok(())
            },
        ),
    );
    handlebars
}

/// Build the Handlebars render context from a notification.
///
/// Populates the developer-guide-required keys for every render:
/// `query_name`, `query_id`, `operation`, `timestamp`, and `metadata`,
/// plus `before` / `after` / `data` as applicable to the operation.
fn build_context(notification: &DefaultChangeNotification) -> Map<String, Value> {
    let mut context = Map::new();

    let query_name = Value::String(notification.query_id.clone());
    context.insert("query_name".to_string(), query_name.clone());
    context.insert("query_id".to_string(), query_name);
    context.insert(
        "operation".to_string(),
        Value::String(notification.op_str().to_string()),
    );
    context.insert(
        "timestamp".to_string(),
        Value::String(notification.timestamp.clone()),
    );
    context.insert(
        "metadata".to_string(),
        notification
            .metadata
            .clone()
            .unwrap_or_else(|| Value::Object(Map::new())),
    );

    match notification.operation {
        Operation::Add => {
            if let Some(after) = &notification.after {
                context.insert("after".to_string(), after.clone());
            }
        }
        Operation::Delete => {
            if let Some(before) = &notification.before {
                context.insert("before".to_string(), before.clone());
            }
        }
        Operation::Update => {
            if let Some(before) = &notification.before {
                context.insert("before".to_string(), before.clone());
            }
            if let Some(after) = &notification.after {
                context.insert("after".to_string(), after.clone());
            }
            if let Some(data) = &notification.raw_data {
                context.insert("data".to_string(), data.clone());
            }
        }
    }

    context
}

/// Render a single batch item for adaptive (coalesced) delivery.
///
/// Resolves the body template for this notification (per-query route → last
/// dotted segment → default template) and renders it to a JSON value. When no
/// body template applies, or rendering produces invalid JSON, falls back to the
/// default [`DefaultChangeNotification`] envelope so an item is never dropped.
///
/// Only the body template applies in batch mode: a coalesced batch is a single
/// POST to the configured `batch_endpoint`, so per-item `url` / `method` /
/// `headers` are not used.
pub(crate) fn render_batch_item(
    handlebars: &Handlebars<'static>,
    config: &HttpReactionConfig,
    notification: &DefaultChangeNotification,
    reaction_name: &str,
) -> Value {
    let query_name = &notification.query_id;
    let spec = config.get_template_spec(query_name, notification.operation_type());

    let template = match spec {
        Some(spec) if !spec.template.is_empty() => &spec.template,
        _ => return default_item(notification),
    };

    let context = build_context(notification);
    match handlebars.render_template(template, &context) {
        Ok(rendered) => match serde_json::from_str::<Value>(&rendered) {
            Ok(value) => value,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Batch body template for query '{query_name}' ({}) rendered invalid JSON: {e} — using default envelope",
                    notification.op_str()
                );
                default_item(notification)
            }
        },
        Err(e) => {
            warn!(
                "[{reaction_name}] Batch body template render failed for query '{query_name}' ({}): {e} — using default envelope",
                notification.op_str()
            );
            default_item(notification)
        }
    }
}

/// Serialize the default change-notification envelope as a JSON value (the
/// batch-item fallback shape).
fn default_item(notification: &DefaultChangeNotification) -> Value {
    serde_json::to_value(notification).unwrap_or(Value::Null)
}

/// Render `call_spec` against `data` and POST/PUT/etc. it to `{base_url}{url}`.
///
/// Runtime render failures are handled by template kind: URL failures use
/// the documented default `/changes/{query_name}` URL, body failures keep
/// the route but use the default notification envelope as the body, and
/// header value failures drop only that header.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_result(
    client: &Client,
    handlebars: &Handlebars<'static>,
    base_url: &str,
    token: &Option<String>,
    call_spec: &HttpCallSpec,
    notification: &DefaultChangeNotification,
    query_name: &str,
    reaction_name: &str,
) -> Result<DeliveryOutcome> {
    let result_type = notification.op_str();
    let context = build_context(notification);

    // Render the URL; on failure fall back to the default change-notification URL.
    let rendered_spec_url = match handlebars.render_template(&call_spec.extension.url, &context) {
        Ok(u) => u,
        Err(e) => {
            warn!(
                "[{reaction_name}] URL template render failed for query '{query_name}' ({result_type}): {e} — using /changes/{query_name}"
            );
            format!("/changes/{query_name}")
        }
    };
    // A rejected URL (SSRF guard / unparseable) is a permanent misconfiguration
    // for this event: drop it rather than fail-stopping and replaying forever.
    // The rendered URL can carry attacker-controllable, templated content, so it
    // is sanitized before logging (and the full value is never emitted raw).
    let full_url = match resolve_http_url(base_url, &rendered_spec_url) {
        Ok(url) => url,
        Err(e) => {
            error!(
                "[{reaction_name}] Rejecting rendered URL for query '{query_name}' \
                 ({result_type}): {} — dropping event",
                truncate_for_log(&format!("{e:#}"))
            );
            return Ok(DeliveryOutcome::Dropped);
        }
    };

    // Render body. Empty template => emit the standard change-notification envelope.
    let body = if !call_spec.template.is_empty() {
        debug!(
            "[{reaction_name}] Rendering body template for query '{query_name}' ({result_type})"
        );
        match handlebars.render_template(&call_spec.template, &context) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Body template render failed for query '{query_name}' ({result_type}): {e} — using default notification envelope"
                );
                serde_json::to_string(notification)?
            }
        }
    } else {
        serde_json::to_string(notification)?
    };

    // Build headers (with optional auth) and render header value templates
    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        match HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(value) => {
                headers.insert("Authorization", value);
            }
            Err(e) => {
                error!(
                    "[{reaction_name}] Invalid auth token for query '{query_name}' ({result_type}): {e} — dropping event"
                );
                return Ok(DeliveryOutcome::Dropped);
            }
        }
    }
    for (key, value) in &call_spec.extension.headers {
        let header_name = match HeaderName::from_bytes(key.as_bytes()) {
            Ok(name) => name,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Header '{key}' is not a valid header name for query '{query_name}' ({result_type}): {e} — dropping header"
                );
                continue;
            }
        };
        let rendered_value = match handlebars.render_template(value, &context) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Header '{key}' template render failed for query '{query_name}' ({result_type}): {e} — dropping header"
                );
                continue;
            }
        };
        let header_value = match HeaderValue::from_str(&rendered_value) {
            Ok(value) => value,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Header '{key}' rendered an invalid value for query '{query_name}' ({result_type}): {e} — dropping header"
                );
                continue;
            }
        };
        headers.insert(header_name, header_value);
    }

    let method = match parse_http_method(&call_spec.extension.method) {
        Ok(method) => method,
        Err(e) => {
            error!(
                "[{reaction_name}] Invalid HTTP method '{}' for query '{query_name}' ({result_type}): {e} — dropping event",
                call_spec.extension.method
            );
            return Ok(DeliveryOutcome::Dropped);
        }
    };

    debug!("[{reaction_name}] Sending {method} request to {full_url}");
    send_with_retry(
        client,
        method,
        full_url,
        headers,
        body,
        reaction_name,
        "HTTP request",
    )
    .await
}

/// `POST {base_url}/changes/{query_name}` with the standard
/// [`DefaultChangeNotification`] envelope as the JSON body.
pub(crate) async fn post_default_notification(
    client: &Client,
    base_url: &str,
    token: &Option<String>,
    notification: &DefaultChangeNotification,
    query_name: &str,
    reaction_name: &str,
) -> Result<DeliveryOutcome> {
    let full_url = format!("{base_url}/changes/{query_name}");
    let body = serde_json::to_string(notification)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        match HeaderValue::from_str(&format!("Bearer {token}")) {
            Ok(value) => {
                headers.insert("Authorization", value);
            }
            Err(e) => {
                error!(
                    "[{reaction_name}] Invalid auth token for query '{query_name}': {e} — dropping event"
                );
                return Ok(DeliveryOutcome::Dropped);
            }
        }
    }

    debug!("[{reaction_name}] Default-notification POST to {full_url}");
    send_with_retry(
        client,
        Method::POST,
        full_url,
        headers,
        body,
        reaction_name,
        "default-notification HTTP request",
    )
    .await
}

pub(crate) async fn send_with_retry(
    client: &Client,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: String,
    reaction_name: &str,
    description: &str,
) -> Result<DeliveryOutcome> {
    let mut backoff = INITIAL_RETRY_BACKOFF;

    for attempt in 1..=MAX_DELIVERY_ATTEMPTS {
        let response = client
            .request(method.clone(), &url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;

        match response {
            Ok(response) => {
                let status = response.status();
                debug!(
                    "[{reaction_name}] {description} {method} {url} attempt {attempt} - Status: {}",
                    status.as_u16()
                );

                if status.is_success() {
                    return Ok(DeliveryOutcome::Delivered);
                }

                // Truncate and sanitize the downstream body before it ever
                // reaches a log line: an adversarial or misconfigured server can
                // return a multi-MB body or embed CRLF to bloat logs / inject
                // fake log lines.
                let error_body = truncate_for_log(
                    &response
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unable to read response body".to_string()),
                );

                match classify_status(status) {
                    StatusClass::Retryable if attempt < MAX_DELIVERY_ATTEMPTS => {
                        warn!(
                            "[{reaction_name}] Transient {description} failure with status {} on attempt {attempt}: {error_body}; retrying",
                            status.as_u16()
                        );
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2);
                        continue;
                    }
                    StatusClass::Retryable => {
                        return Err(anyhow!(
                            "{description} failed after {MAX_DELIVERY_ATTEMPTS} attempts with status {}: {error_body}",
                            status.as_u16()
                        ));
                    }
                    StatusClass::AuthReject => {
                        // Auth/permission rejection (401/403/407). Retrying the
                        // same request in place won't help, but the event must
                        // NOT be dropped: the credential can be refreshed and the
                        // event replayed from the outbox. Surface as a sustained
                        // failure so the reaction's recovery policy applies
                        // (Strict fail-stops; AutoSkipGap skips).
                        return Err(anyhow!(
                            "{description} rejected with status {} ({error_body}); auth/permission \
                             failure — applying recovery policy rather than dropping the event",
                            status.as_u16()
                        ));
                    }
                    StatusClass::Permanent => {
                        error!(
                            "[{reaction_name}] Permanent {description} failure with status {}: {error_body}; dropping event",
                            status.as_u16()
                        );
                        return Ok(DeliveryOutcome::Dropped);
                    }
                }
            }
            Err(e) if attempt < MAX_DELIVERY_ATTEMPTS => {
                warn!(
                    "[{reaction_name}] Transient {description} send error on attempt {attempt}: {e}; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
            }
            Err(e) => {
                return Err(anyhow!(
                    "{description} failed after {MAX_DELIVERY_ATTEMPTS} attempts: {e}"
                ));
            }
        }
    }

    unreachable!("retry loop always returns")
}

/// How a non-2xx HTTP response status should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusClass {
    /// Transient — retry in place, then surface as a sustained failure if the
    /// retry budget is exhausted (5xx, 408, 409, 425, 429).
    Retryable,
    /// Auth/permission rejection (401, 403, 407). Retrying the same request in
    /// place won't help, but the event must not be dropped — refreshing the
    /// credential and replaying fixes it. Surfaced as a sustained failure so the
    /// recovery policy applies.
    AuthReject,
    /// Genuinely permanent for this event (other 4xx — 400/404/405/422/…). The
    /// event is poison: dropped, with the checkpoint advancing past it.
    Permanent,
}

fn classify_status(status: StatusCode) -> StatusClass {
    if is_retryable_status(status) {
        StatusClass::Retryable
    } else if matches!(
        status,
        StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::PROXY_AUTHENTICATION_REQUIRED
    ) {
        StatusClass::AuthReject
    } else {
        StatusClass::Permanent
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status.as_u16() == 425
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::CONFLICT | StatusCode::TOO_MANY_REQUESTS
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::DefaultChangeNotification;
    use chrono::TimeZone;
    use drasi_lib::channels::{QueryResult, ResultDiff};
    use serde_json::json;
    use std::collections::HashMap;

    fn notification(
        metadata: HashMap<String, Value>,
        diff: ResultDiff,
    ) -> DefaultChangeNotification {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let qr = QueryResult::new("source.q1".to_string(), 7, ts, vec![diff], metadata);
        DefaultChangeNotification::from_diff(&qr, &qr.results[0]).unwrap()
    }

    #[test]
    fn context_for_add_has_required_keys_and_after() {
        let n = notification(
            HashMap::new(),
            ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        );
        let ctx = build_context(&n);
        assert_eq!(ctx.get("query_name"), Some(&json!("source.q1")));
        assert_eq!(ctx.get("query_id"), Some(&json!("source.q1")));
        assert_eq!(ctx.get("operation"), Some(&json!("ADD")));
        assert!(ctx.get("timestamp").and_then(|t| t.as_str()).is_some());
        assert_eq!(ctx.get("metadata"), Some(&json!({})));
        assert_eq!(ctx.get("after"), Some(&json!({"id": 1})));
        assert!(ctx.get("before").is_none(), "ADD has no before");
        assert!(ctx.get("data").is_none(), "ADD has no data key");
    }

    #[test]
    fn context_for_delete_has_before_only() {
        let n = notification(
            HashMap::new(),
            ResultDiff::Delete {
                data: json!({"id": 2}),
                row_signature: 0,
            },
        );
        let ctx = build_context(&n);
        assert_eq!(ctx.get("operation"), Some(&json!("DELETE")));
        assert_eq!(ctx.get("before"), Some(&json!({"id": 2})));
        assert!(ctx.get("after").is_none(), "DELETE has no after");
    }

    #[test]
    fn context_for_update_has_before_after_and_data() {
        let n = notification(
            HashMap::new(),
            ResultDiff::Update {
                data: json!({"raw": true}),
                before: json!({"v": 1}),
                after: json!({"v": 2}),
                grouping_keys: None,
                row_signature: 0,
            },
        );
        let ctx = build_context(&n);
        assert_eq!(ctx.get("operation"), Some(&json!("UPDATE")));
        assert_eq!(ctx.get("before"), Some(&json!({"v": 1})));
        assert_eq!(ctx.get("after"), Some(&json!({"v": 2})));
        assert_eq!(ctx.get("data"), Some(&json!({"raw": true})));
    }

    #[test]
    fn context_carries_non_empty_metadata() {
        let mut metadata = HashMap::new();
        metadata.insert("source".to_string(), json!("sensors"));
        let n = notification(
            metadata,
            ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        );
        let ctx = build_context(&n);
        assert_eq!(ctx.get("metadata"), Some(&json!({"source": "sensors"})));
    }

    #[test]
    fn parse_method_is_case_insensitive_and_empty_defaults_to_post() {
        assert_eq!(parse_http_method("get").unwrap(), Method::GET);
        assert_eq!(parse_http_method("Put").unwrap(), Method::PUT);
        assert_eq!(parse_http_method("PATCH").unwrap(), Method::PATCH);
        assert_eq!(parse_http_method("delete").unwrap(), Method::DELETE);
        assert_eq!(parse_http_method("").unwrap(), Method::POST);
        assert!(parse_http_method("nonsense").is_err());
    }

    #[test]
    fn retryable_status_classification() {
        for code in [500u16, 502, 503, 504, 408, 409, 425, 429] {
            assert!(
                is_retryable_status(StatusCode::from_u16(code).unwrap()),
                "{code} should be retryable"
            );
        }
        for code in [200u16, 201, 204, 400, 401, 403, 404, 422] {
            assert!(
                !is_retryable_status(StatusCode::from_u16(code).unwrap()),
                "{code} should NOT be retryable"
            );
        }
    }

    #[test]
    fn truncate_for_log_caps_length_and_strips_control_chars() {
        // CRLF and other control characters are replaced with spaces (no log
        // injection); a normal short string is returned unchanged.
        assert_eq!(truncate_for_log("ok"), "ok");
        assert_eq!(truncate_for_log("a\r\nb\tc"), "a  b c");

        // Over-long input is truncated and marked with an ellipsis.
        let long = "x".repeat(MAX_LOG_FIELD_LEN + 50);
        let out = truncate_for_log(&long);
        assert_eq!(out.chars().count(), MAX_LOG_FIELD_LEN + 1); // +1 for '…'
        assert!(out.ends_with('…'));
    }
}
