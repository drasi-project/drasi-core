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

use anyhow::{bail, Result};
use handlebars::Handlebars;
use log::{debug, warn};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method,
};
use serde_json::{Map, Value};

use crate::config::HttpCallSpec;
use crate::output::{DefaultChangeNotification, Operation};

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

/// Render `call_spec` against `data` and POST/PUT/etc. it to `{base_url}{url}`.
///
/// On any render error (URL, body, or header), falls back to a `POST
/// {base_url}/changes/{query_name}` carrying `notification` as the body —
/// events are **never dropped** because a template failed. The same
/// notification is also sent as the body when `call_spec.template` is
/// empty (i.e., the caller did not specify a body template).
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
) -> Result<()> {
    let result_type = notification.op_str();
    let context = build_context(notification);

    // Render the URL; on failure fall back to the default change-notification endpoint.
    let rendered_spec_url = match handlebars.render_template(&call_spec.extension.url, &context) {
        Ok(u) => u,
        Err(e) => {
            warn!(
                "[{reaction_name}] URL template render failed for query '{query_name}' ({result_type}): {e} — falling back to /changes/{query_name}"
            );
            return post_default_notification(
                client,
                base_url,
                token,
                notification,
                query_name,
                reaction_name,
            )
            .await;
        }
    };
    let full_url =
        if rendered_spec_url.starts_with("http://") || rendered_spec_url.starts_with("https://") {
            // SSRF guard: a rendered absolute URL may incorporate graph-data
            // fields. Only allow it when its host matches the configured
            // base_url host; otherwise reject the event.
            let base_host = reqwest::Url::parse(base_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned));
            let resolved_host = reqwest::Url::parse(&rendered_spec_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned));
            if base_host.is_none() || base_host != resolved_host {
                bail!(
                    "[{reaction_name}] Rendered URL '{rendered_spec_url}' host does not match \
                     base_url '{base_url}' host for query '{query_name}' — rejecting request"
                );
            }
            rendered_spec_url
        } else {
            format!("{base_url}{rendered_spec_url}")
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
                    "[{reaction_name}] Body template render failed for query '{query_name}' ({result_type}): {e} — falling back to /changes/{query_name}"
                );
                return post_default_notification(
                    client,
                    base_url,
                    token,
                    notification,
                    query_name,
                    reaction_name,
                )
                .await;
            }
        }
    } else {
        serde_json::to_string(notification)?
    };

    // Build headers (with optional auth) and render header value templates
    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }
    for (key, value) in &call_spec.extension.headers {
        let header_name = HeaderName::from_bytes(key.as_bytes())?;
        let rendered_value = match handlebars.render_template(value, &context) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Header '{key}' template render failed for query '{query_name}' ({result_type}): {e} — falling back to /changes/{query_name}"
                );
                return post_default_notification(
                    client,
                    base_url,
                    token,
                    notification,
                    query_name,
                    reaction_name,
                )
                .await;
            }
        };
        let header_value = HeaderValue::from_str(&rendered_value)?;
        headers.insert(header_name, header_value);
    }

    let method = parse_method(&call_spec.extension.method);

    debug!("[{reaction_name}] Sending {method} request to {full_url}");
    let response = client
        .request(method.clone(), &full_url)
        .headers(headers)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    debug!(
        "[{reaction_name}] HTTP {method} {full_url} - Status: {}",
        status.as_u16()
    );

    if !status.is_success() {
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read response body".to_string());
        warn!(
            "[{reaction_name}] HTTP request failed with status {}: {error_body}",
            status.as_u16()
        );
    }

    Ok(())
}

fn parse_method(method: &str) -> Method {
    match method.to_uppercase().as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "DELETE" => Method::DELETE,
        "PATCH" => Method::PATCH,
        _ => Method::POST,
    }
}

/// `POST {base_url}/changes/{query_name}` with the standard
/// [`DefaultChangeNotification`] envelope as the JSON body. Used both by
/// the no-template default delivery path and as the render-error fallback
/// (URL/body/header template failure) so events are never dropped.
pub(crate) async fn post_default_notification(
    client: &Client,
    base_url: &str,
    token: &Option<String>,
    notification: &DefaultChangeNotification,
    query_name: &str,
    reaction_name: &str,
) -> Result<()> {
    let full_url = format!("{base_url}/changes/{query_name}");
    let body = serde_json::to_string(notification)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }

    debug!("[{reaction_name}] Default-notification POST to {full_url}");
    let response = client
        .post(&full_url)
        .headers(headers)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read response body".to_string());
        warn!(
            "[{reaction_name}] Default-notification HTTP request failed with status {}: {error_body}",
            status.as_u16()
        );
    }

    Ok(())
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
    fn parse_method_is_case_insensitive_and_defaults_to_post() {
        assert_eq!(parse_method("get"), Method::GET);
        assert_eq!(parse_method("Put"), Method::PUT);
        assert_eq!(parse_method("PATCH"), Method::PATCH);
        assert_eq!(parse_method("delete"), Method::DELETE);
        assert_eq!(parse_method("nonsense"), Method::POST);
        assert_eq!(parse_method(""), Method::POST);
    }
}
