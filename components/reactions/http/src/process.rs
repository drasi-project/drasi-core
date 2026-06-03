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

use crate::config::{synthesized_default_spec, HttpCallSpec};

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

fn build_context(result_type: &str, data: &Value, query_name: &str) -> Map<String, Value> {
    let mut context = Map::new();

    match result_type {
        "ADD" => {
            context.insert("after".to_string(), data.clone());
        }
        "UPDATE" => {
            if let Some(obj) = data.as_object() {
                if let Some(before) = obj.get("before") {
                    context.insert("before".to_string(), before.clone());
                }
                if let Some(after) = obj.get("after") {
                    context.insert("after".to_string(), after.clone());
                }
                if let Some(data_field) = obj.get("data") {
                    context.insert("data".to_string(), data_field.clone());
                }
            } else {
                context.insert("after".to_string(), data.clone());
            }
        }
        "DELETE" => {
            context.insert("before".to_string(), data.clone());
        }
        _ => {
            context.insert("data".to_string(), data.clone());
        }
    }

    context.insert(
        "query_name".to_string(),
        Value::String(query_name.to_string()),
    );
    context.insert(
        "operation".to_string(),
        Value::String(result_type.to_string()),
    );

    context
}

/// Render `call_spec` against `data` and POST/PUT/etc. it to `{base_url}{url}`.
///
/// On any render error, falls back to a raw `POST {base_url}/changes/{query_name}`
/// with the raw `data` JSON — events are **never dropped** because a
/// template failed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_result(
    client: &Client,
    handlebars: &Handlebars<'static>,
    base_url: &str,
    token: &Option<String>,
    call_spec: &HttpCallSpec,
    result_type: &str,
    data: &Value,
    query_name: &str,
    reaction_name: &str,
) -> Result<()> {
    let context = build_context(result_type, data, query_name);

    // Render the URL; on failure fall back to the synthesized default.
    let rendered_spec_url = match handlebars.render_template(&call_spec.extension.url, &context) {
        Ok(u) => u,
        Err(e) => {
            warn!(
                "[{reaction_name}] URL template render failed for query '{query_name}' ({result_type}): {e} — falling back to /changes/{query_name}"
            );
            return fallback_post(client, base_url, token, data, query_name, reaction_name).await;
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

    // Render body
    let body = if !call_spec.template.is_empty() {
        debug!("[{reaction_name}] Rendering body template for query '{query_name}' ({result_type})");
        match handlebars.render_template(&call_spec.template, &context) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "[{reaction_name}] Body template render failed for query '{query_name}' ({result_type}): {e} — falling back to /changes/{query_name}"
                );
                return fallback_post(client, base_url, token, data, query_name, reaction_name)
                    .await;
            }
        }
    } else {
        serde_json::to_string(&data)?
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
                return fallback_post(client, base_url, token, data, query_name, reaction_name)
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

async fn fallback_post(
    client: &Client,
    base_url: &str,
    token: &Option<String>,
    data: &Value,
    query_name: &str,
    reaction_name: &str,
) -> Result<()> {
    let spec = synthesized_default_spec(query_name);
    let full_url = format!("{base_url}{}", spec.extension.url);
    let body = serde_json::to_string(data)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }

    debug!("[{reaction_name}] Fallback POST to {full_url}");
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
            "[{reaction_name}] Fallback HTTP request failed with status {}: {error_body}",
            status.as_u16()
        );
    }

    Ok(())
}
