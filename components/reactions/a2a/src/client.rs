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

use std::time::Duration;

use anyhow::{anyhow, Context};
use log::warn;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_DELIVERY_ATTEMPTS: usize = 3;
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq)]
pub enum OutboundPart {
    Text(String),
    Data { media_type: String, data: Value },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendMessageRequest {
    pub message_id: String,
    pub activation_id: String,
    pub task_id: Option<String>,
    pub parts: Vec<OutboundPart>,
    pub return_immediately: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CancelTaskRequest {
    pub message_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendMessageResult {
    Task {
        task_id: String,
        context_id: String,
        state: String,
        terminal: bool,
    },
    Message,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryResult<T> {
    Delivered(T),
    Dropped,
}

#[derive(Clone)]
pub struct A2AClient {
    endpoint: String,
    token: Option<String>,
    http: Client,
}

impl A2AClient {
    pub fn new(endpoint: String, token: Option<String>, timeout_ms: u64) -> anyhow::Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .build()
            .context("failed to build reqwest client for A2A reaction")?;
        Ok(Self {
            endpoint,
            token,
            http,
        })
    }

    pub async fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> anyhow::Result<DeliveryResult<SendMessageResult>> {
        let body = build_send_message_rpc(&request);
        let response = self.send_rpc(&body, "SendMessage").await?;
        let Some(result) = response else {
            return Ok(DeliveryResult::Dropped);
        };
        let parsed = parse_send_message_result(&result)
            .context("failed to parse SendMessage JSON-RPC result")?;
        Ok(DeliveryResult::Delivered(parsed))
    }

    pub async fn cancel_task(
        &self,
        request: CancelTaskRequest,
    ) -> anyhow::Result<DeliveryResult<()>> {
        let body = build_cancel_task_rpc(&request);
        let response = self.send_rpc(&body, "CancelTask").await?;
        match response {
            Some(_) => Ok(DeliveryResult::Delivered(())),
            None => Ok(DeliveryResult::Dropped),
        }
    }

    async fn send_rpc(&self, payload: &Value, method_name: &str) -> anyhow::Result<Option<Value>> {
        let mut backoff = INITIAL_RETRY_BACKOFF;
        for attempt in 1..=MAX_DELIVERY_ATTEMPTS {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            if let Some(token) = &self.token {
                let mut auth_header = String::from("Bearer ");
                auth_header.push_str(token);
                let auth_value = HeaderValue::from_str(&auth_header)
                    .context("failed to encode Authorization header value")?;
                headers.insert("authorization", auth_value);
            }

            let response = self
                .http
                .post(&self.endpoint)
                .headers(headers)
                .json(payload)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let rpc: JsonRpcResponse = response
                            .json()
                            .await
                            .context("failed to decode JSON-RPC response body")?;
                        if let Some(error) = rpc.error {
                            return Err(anyhow!(
                                "A2A {method_name} returned JSON-RPC error {}: {}",
                                error.code,
                                error.message
                            ));
                        }
                        return Ok(rpc.result);
                    }

                    if is_retryable_status(status) {
                        if attempt < MAX_DELIVERY_ATTEMPTS {
                            warn!(
                                "A2A {method_name} HTTP status {} on attempt {attempt}, retrying",
                                status.as_u16()
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = backoff.saturating_mul(2);
                            continue;
                        }
                        anyhow::bail!(
                            "A2A {method_name} failed with retryable status {} after {} attempts",
                            status.as_u16(),
                            MAX_DELIVERY_ATTEMPTS
                        );
                    }

                    return Ok(None);
                }
                Err(error) => {
                    if attempt < MAX_DELIVERY_ATTEMPTS {
                        warn!("A2A {method_name} transport error on attempt {attempt}: {error}");
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2);
                        continue;
                    }
                    return Err(error).context(format!(
                        "A2A {method_name} failed after {MAX_DELIVERY_ATTEMPTS} transport attempts"
                    ));
                }
            }
        }

        unreachable!("retry loop exits with return on all branches")
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || status.as_u16() == 425
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT
                | StatusCode::CONFLICT
                | StatusCode::TOO_MANY_REQUESTS
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::PROXY_AUTHENTICATION_REQUIRED
        )
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendMessageConfiguration {
    return_immediately: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendMessageParams {
    message: SendMessagePayload,
    configuration: SendMessageConfiguration,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SendMessagePayload {
    role: &'static str,
    message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    parts: Vec<WirePart>,
    metadata: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum WirePart {
    Text {
        text: String,
    },
    Data {
        data: Value,
        #[serde(rename = "mediaType")]
        media_type: String,
    },
}

pub fn build_send_message_rpc(request: &SendMessageRequest) -> Value {
    let parts = request
        .parts
        .iter()
        .map(|part| match part {
            OutboundPart::Text(text) => WirePart::Text { text: text.clone() },
            OutboundPart::Data { media_type, data } => WirePart::Data {
                media_type: media_type.clone(),
                data: data.clone(),
            },
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request.message_id,
        "method": "SendMessage",
        "params": SendMessageParams {
            message: SendMessagePayload {
                role: "ROLE_USER",
                message_id: request.message_id.clone(),
                task_id: request.task_id.clone(),
                parts,
                metadata: serde_json::json!({ "activationId": request.activation_id }),
            },
            configuration: SendMessageConfiguration {
                return_immediately: request.return_immediately,
            },
        },
    })
}

pub fn build_cancel_task_rpc(request: &CancelTaskRequest) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request.message_id,
        "method": "CancelTask",
        "params": { "id": request.task_id },
    })
}

pub fn parse_send_message_result(value: &Value) -> anyhow::Result<SendMessageResult> {
    if let Some(task) = value.get("task") {
        return parse_task(task);
    }
    if let Some(message) = value.get("message") {
        if looks_like_message(message) {
            return Ok(SendMessageResult::Message);
        }
        return Err(anyhow!(
            "SendMessage result.message did not match Message shape: {value}"
        ));
    }
    if looks_like_task(value) {
        return parse_task(value);
    }
    if looks_like_message(value) {
        return Ok(SendMessageResult::Message);
    }

    Err(anyhow!(
        "SendMessage result did not match Task or Message shape: {value}"
    ))
}

fn looks_like_task(value: &Value) -> bool {
    value.get("id").and_then(Value::as_str).is_some() && task_state(value).is_some()
}

fn looks_like_message(value: &Value) -> bool {
    value.get("parts").is_some()
}

fn task_state(value: &Value) -> Option<&str> {
    value
        .get("status")
        .and_then(|status| status.get("state"))
        .and_then(Value::as_str)
        .or_else(|| value.get("state").and_then(Value::as_str))
}

fn parse_task(value: &Value) -> anyhow::Result<SendMessageResult> {
    let Some(task_id) = value.get("id").and_then(Value::as_str) else {
        return Err(anyhow!("SendMessage Task result is missing id: {value}"));
    };
    let Some(state) = task_state(value) else {
        return Err(anyhow!(
            "SendMessage Task result is missing status.state: {value}"
        ));
    };

    let normalized_state = normalize_task_state(state);
    let terminal = is_terminal_state(&normalized_state);
    let context_id = value
        .get("contextId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Ok(SendMessageResult::Task {
        task_id: task_id.to_string(),
        context_id,
        state: normalized_state,
        terminal,
    })
}

fn normalize_task_state(state: &str) -> String {
    state
        .trim()
        .trim_start_matches("TASK_STATE_")
        .to_ascii_uppercase()
}

fn is_terminal_state(state: &str) -> bool {
    matches!(state, "COMPLETED" | "FAILED" | "CANCELED" | "REJECTED")
}
