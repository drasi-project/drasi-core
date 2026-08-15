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

//! Signed webhook listener and durable inbox admission pipeline.

use crate::types::{HydratorHealth, WebhookLocator};
use anyhow::{anyhow, Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::wal::{WalError, WalProvider};
use hmac::{Hmac, Mac};
use log::{debug, error, warn};
use serde_json::json;
use sha2::Sha256;
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, RwLock};

type HmacSha256 = Hmac<Sha256>;

const DELIVERY_LABEL: &str = "__GitHubDelivery";

#[derive(Clone)]
pub struct WebhookServerParams {
    pub source_id: String,
    pub inbox_source_id: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub body_limit_bytes: usize,
    pub secret: String,
    pub wal: Arc<dyn WalProvider>,
    pub hydrator_notify: Arc<Notify>,
    pub hydrator_health: Arc<RwLock<HydratorHealth>>,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

#[derive(Clone)]
struct WebhookState {
    source_id: String,
    inbox_source_id: String,
    path: String,
    secret: Vec<u8>,
    wal: Arc<dyn WalProvider>,
    hydrator_notify: Arc<Notify>,
    hydrator_health: Arc<RwLock<HydratorHealth>>,
    ingress_gate: Arc<Mutex<()>>,
}

pub async fn run_webhook_server(params: WebhookServerParams) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", params.host, params.port)
        .parse()
        .with_context(|| {
            format!(
                "Invalid webhook bind address {}:{}",
                params.host, params.port
            )
        })?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind webhook listener on {addr}"))?;
    serve_webhook_listener(listener, params).await
}

pub async fn serve_webhook_listener(
    listener: TcpListener,
    params: WebhookServerParams,
) -> Result<()> {
    let state = Arc::new(WebhookState {
        source_id: params.source_id.clone(),
        inbox_source_id: params.inbox_source_id,
        path: params.path.clone(),
        secret: params.secret.into_bytes(),
        wal: params.wal,
        hydrator_notify: params.hydrator_notify,
        hydrator_health: params.hydrator_health,
        ingress_gate: Arc::new(Mutex::new(())),
    });

    let router = Router::new()
        .route(&params.path, post(webhook_handler))
        .route("/health", get(health_handler))
        .layer(axum::extract::DefaultBodyLimit::max(
            params.body_limit_bytes,
        ))
        .with_state(state);

    let mut shutdown = params.shutdown.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .context("Webhook server exited with error")
}

async fn webhook_handler(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    match handle_delivery(state, headers, body).await {
        Ok(status) => (status, Json(json!({ "status": "ok" }))).into_response(),
        Err(DeliveryError::Unauthorized(msg)) => {
            (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg }))).into_response()
        }
        Err(DeliveryError::BadRequest(msg)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
        }
        Err(DeliveryError::ServiceUnavailable(msg)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": msg })),
        )
            .into_response(),
    }
}

async fn health_handler(State(state): State<Arc<WebhookState>>) -> impl IntoResponse {
    let health = state.hydrator_health.read().await.clone();
    let degraded = health.terminal || health.stalled_delivery_id.is_some();
    let status = if degraded { "degraded" } else { "ok" };
    let code = if degraded {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (
        code,
        Json(json!({
            "status": status,
            "sourceId": state.source_id,
            "path": state.path,
            "hydrator": health
        })),
    )
}

#[derive(Debug)]
pub(crate) enum DeliveryError {
    Unauthorized(String),
    BadRequest(String),
    ServiceUnavailable(String),
}

async fn handle_delivery(
    state: Arc<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, DeliveryError> {
    let signature = header_str(&headers, "x-hub-signature-256").ok_or_else(|| {
        DeliveryError::Unauthorized("Missing X-Hub-Signature-256 header".to_string())
    })?;
    verify_signature(&state.secret, &body, signature)
        .map_err(|e| DeliveryError::Unauthorized(e.to_string()))?;

    let delivery_id = header_str(&headers, "x-github-delivery")
        .ok_or_else(|| DeliveryError::BadRequest("Missing X-GitHub-Delivery header".to_string()))?;
    let event_type = header_str(&headers, "x-github-event")
        .ok_or_else(|| DeliveryError::BadRequest("Missing X-GitHub-Event header".to_string()))?;

    let locator = parse_locator(event_type, &body)
        .map_err(|e| DeliveryError::BadRequest(format!("Invalid webhook payload: {e}")))?;
    let admission_change = encode_admission_change(&state.source_id, delivery_id, &locator)
        .map_err(|e| {
            DeliveryError::ServiceUnavailable(format!("Failed to encode delivery: {e}"))
        })?;

    let _guard = state.ingress_gate.lock().await;
    {
        let health = state.hydrator_health.read().await;
        if health.terminal {
            return Err(DeliveryError::ServiceUnavailable(
                "Hydrator is unavailable; stop then restart the source".to_string(),
            ));
        }
    }

    if let Some(existing_sequence) =
        find_delivery_in_wal(state.wal.as_ref(), &state.inbox_source_id, delivery_id)
            .await
            .map_err(|e| {
                error!(
                    "[{}] Failed scanning inbox WAL for duplicate delivery {}: {e:#}",
                    state.source_id, delivery_id
                );
                DeliveryError::ServiceUnavailable("Inbox dedupe check failed".to_string())
            })?
    {
        debug!(
            "[{}] Duplicate webhook delivery ignored: {} (already in inbox at seq {})",
            state.source_id, delivery_id, existing_sequence
        );
        return Ok(StatusCode::OK);
    }

    match state
        .wal
        .append(&state.inbox_source_id, &admission_change)
        .await
    {
        Ok(seq) => {
            debug!(
                "[{}] Admitted delivery {} to inbox WAL at seq {}",
                state.source_id, delivery_id, seq
            );
            state.hydrator_notify.notify_one();
            Ok(StatusCode::OK)
        }
        Err(WalError::CapacityExhausted(_)) => Err(DeliveryError::ServiceUnavailable(
            "Inbox WAL capacity exhausted; retry later".to_string(),
        )),
        Err(e) => {
            error!(
                "[{}] Failed to append delivery {} to inbox WAL: {}",
                state.source_id, delivery_id, e
            );
            Err(DeliveryError::ServiceUnavailable(
                "Inbox WAL append failed".to_string(),
            ))
        }
    }
}

pub(crate) async fn find_delivery_in_wal(
    wal: &dyn WalProvider,
    wal_id: &str,
    delivery_id: &str,
) -> Result<Option<u64>> {
    let Some(oldest) = wal.oldest_sequence(wal_id).await? else {
        return Ok(None);
    };
    let entries = wal.read_from(wal_id, oldest).await?;
    for (sequence, change) in entries {
        if let Ok((existing_delivery_id, _)) = decode_admission_change(&change) {
            if existing_delivery_id == delivery_id {
                return Ok(Some(sequence));
            }
        }
    }
    Ok(None)
}

fn header_str<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers
        .get(key)
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
}

pub fn verify_signature(secret: &[u8], body: &[u8], signature_header: &str) -> Result<()> {
    let expected_hex = signature_header
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow!("signature must start with 'sha256='"))?;
    let expected = hex::decode(expected_hex).context("signature hex decode failed")?;

    let mut mac = HmacSha256::new_from_slice(secret).context("invalid HMAC secret")?;
    mac.update(body);
    let actual = mac.finalize().into_bytes().to_vec();

    if actual.as_slice().ct_eq(expected.as_slice()).unwrap_u8() == 1 {
        Ok(())
    } else {
        Err(anyhow!("signature mismatch"))
    }
}

pub(crate) fn parse_locator(event_type: &str, body: &[u8]) -> Result<WebhookLocator> {
    let payload: serde_json::Value = serde_json::from_slice(body)?;
    let action = payload
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let repository_full_name = payload
        .get("repository")
        .and_then(|v| v.get("full_name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());

    let node_id = match event_type {
        "issues" => payload.pointer("/issue/node_id"),
        "pull_request" => payload.pointer("/pull_request/node_id"),
        "issue_comment" => payload
            .pointer("/comment/node_id")
            .or_else(|| payload.pointer("/issue_comment/node_id")),
        "pull_request_review" => payload.pointer("/review/node_id"),
        "pull_request_review_comment" => payload
            .pointer("/comment/node_id")
            .or_else(|| payload.pointer("/review_comment/node_id")),
        "projects_v2_item" => payload
            .pointer("/projects_v2_item/node_id")
            .or_else(|| payload.pointer("/project_item/node_id")),
        "projects_v2" => payload
            .pointer("/projects_v2/node_id")
            .or_else(|| payload.pointer("/project/node_id")),
        "repository" => payload.pointer("/repository/node_id"),
        _ => payload
            .pointer("/comment/node_id")
            .or_else(|| payload.pointer("/review/node_id"))
            .or_else(|| payload.pointer("/issue/node_id"))
            .or_else(|| payload.pointer("/pull_request/node_id"))
            .or_else(|| payload.pointer("/projects_v2_item/node_id"))
            .or_else(|| payload.pointer("/project_item/node_id"))
            .or_else(|| payload.pointer("/projects_v2/node_id"))
            .or_else(|| payload.pointer("/project/node_id"))
            .or_else(|| payload.pointer("/repository/node_id")),
    }
    .and_then(|v| v.as_str())
    .map(str::to_string);

    let parent_issue_id = payload
        .pointer("/issue/node_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let parent_pull_request_id = payload
        .pointer("/pull_request/node_id")
        .or_else(|| payload.pointer("/review/pull_request/node_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let project_id = payload
        .pointer("/projects_v2_item/project_node_id")
        .or_else(|| payload.pointer("/project_item/project_node_id"))
        .or_else(|| payload.pointer("/projects_v2/node_id"))
        .or_else(|| payload.pointer("/project/node_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let project_owner = payload
        .pointer("/projects_v2/owner/login")
        .or_else(|| payload.pointer("/project/owner/login"))
        .or_else(|| payload.pointer("/organization/login"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let project_number = payload
        .pointer("/projects_v2/number")
        .or_else(|| payload.pointer("/project/number"))
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok());

    Ok(WebhookLocator {
        event_type: event_type.to_string(),
        action,
        node_id,
        repository_full_name,
        parent_issue_id,
        parent_pull_request_id,
        project_id,
        project_owner,
        project_number,
    })
}

pub fn encode_admission_change(
    source_id: &str,
    delivery_id: &str,
    locator: &WebhookLocator,
) -> Result<SourceChange> {
    let mut props = ElementPropertyMap::new();
    props.insert("deliveryId", ElementValue::String(Arc::from(delivery_id)));
    props.insert(
        "admittedAt",
        ElementValue::Integer(chrono::Utc::now().timestamp_millis()),
    );
    props.insert(
        "eventType",
        ElementValue::String(Arc::from(locator.event_type.as_str())),
    );
    props.insert(
        "action",
        ElementValue::String(Arc::from(locator.action.as_str())),
    );
    if let Some(node_id) = &locator.node_id {
        props.insert("nodeId", ElementValue::String(Arc::from(node_id.as_str())));
    } else {
        props.insert("nodeId", ElementValue::Null);
    }
    if let Some(repo) = &locator.repository_full_name {
        props.insert(
            "repositoryFullName",
            ElementValue::String(Arc::from(repo.as_str())),
        );
    } else {
        props.insert("repositoryFullName", ElementValue::Null);
    }
    let locator_json = serde_json::to_string(locator)?;
    props.insert(
        "locatorJson",
        ElementValue::String(Arc::from(locator_json.as_str())),
    );

    Ok(SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &format!("delivery:{delivery_id}")),
                labels: Arc::from(vec![Arc::from(DELIVERY_LABEL)]),
                effective_from: chrono::Utc::now().timestamp_millis() as u64,
            },
            properties: props,
        },
    })
}

pub fn decode_admission_change(change: &SourceChange) -> Result<(String, WebhookLocator)> {
    let (SourceChange::Insert { element } | SourceChange::Update { element }) = change else {
        return Err(anyhow!("Unsupported WAL admission record type"));
    };
    let Element::Node {
        metadata: _,
        properties,
    } = element
    else {
        return Err(anyhow!("WAL admission record is not a node"));
    };
    let delivery_id = match properties.get("deliveryId") {
        Some(ElementValue::String(s)) => s.to_string(),
        _ => return Err(anyhow!("WAL admission record missing deliveryId")),
    };
    let locator_json = match properties.get("locatorJson") {
        Some(ElementValue::String(s)) => s.to_string(),
        _ => return Err(anyhow!("WAL admission record missing locatorJson")),
    };
    let locator: WebhookLocator = serde_json::from_str(&locator_json)?;
    Ok((delivery_id, locator))
}

pub fn delivery_label() -> &'static str {
    DELIVERY_LABEL
}

pub fn warn_unhealthy_hydrator(source_id: &str, health: &HydratorHealth) {
    if let Some(stalled_id) = &health.stalled_delivery_id {
        warn!(
            "[{}] Hydrator stalled on delivery {} (retry_count={}, next_retry_secs={:?}, error={:?})",
            source_id, stalled_id, health.retry_count, health.next_retry_secs, health.last_error
        );
    }
}
