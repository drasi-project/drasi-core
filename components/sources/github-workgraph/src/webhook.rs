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

use crate::config::RepositoryFilter;
use crate::mapping::{ConvertError, Converter};
use anyhow::{anyhow, Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::{WalError, WalProvider};
use hmac::{Hmac, Mac};
use log::{debug, error, warn};
use serde_json::json;
use sha2::Sha256;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

type HmacSha256 = Hmac<Sha256>;

const DELIVERY_KEY_PREFIX: &str = "delivery:";

pub struct IngressParams {
    pub source_id: String,
    pub organization: String,
    pub repository_filter: RepositoryFilter,
    pub path: String,
    pub secret: String,
    pub body_limit_bytes: usize,
    pub wal: Arc<dyn WalProvider>,
    pub state_store: Arc<dyn StateStoreProvider>,
    pub notify: Arc<Notify>,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

struct IngressState {
    source_id: String,
    organization: String,
    repository_filter: RepositoryFilter,
    secret: Vec<u8>,
    wal: Arc<dyn WalProvider>,
    state_store: Arc<dyn StateStoreProvider>,
    notify: Arc<Notify>,
    gate: Mutex<()>,
}

pub async fn serve(listener: TcpListener, params: IngressParams) -> Result<()> {
    let state = Arc::new(IngressState {
        source_id: params.source_id,
        organization: params.organization,
        repository_filter: params.repository_filter,
        secret: params.secret.into_bytes(),
        wal: params.wal,
        state_store: params.state_store,
        notify: params.notify,
        gate: Mutex::new(()),
    });
    let router = Router::new()
        .route(&params.path, post(handler))
        .layer(axum::extract::DefaultBodyLimit::max(
            params.body_limit_bytes,
        ))
        .with_state(state);
    let mut shutdown = params.shutdown;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .context("GitHub WorkGraph webhook server exited with error")
}

type Rejection = (StatusCode, String);

fn reject<T>(code: StatusCode, message: impl Into<String>) -> Result<T, Rejection> {
    Err((code, message.into()))
}

fn store_unavailable(source_id: &str, error: impl std::fmt::Display) -> Rejection {
    error!("[{source_id}] delivery dedupe store unavailable: {error}");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "delivery dedupe store unavailable".to_string(),
    )
}

async fn handler(
    State(state): State<Arc<IngressState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match handle_delivery(&state, &headers, &body).await {
        Ok(Some(count)) => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": "accepted", "changes": count })),
        )
            .into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err((code, message)) => (code, Json(json!({ "error": message }))).into_response(),
    }
}

async fn handle_delivery(
    state: &IngressState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<usize>, Rejection> {
    let unauthorized = |m: String| (StatusCode::UNAUTHORIZED, m);
    let bad_request = |m: String| (StatusCode::BAD_REQUEST, m);
    let signature = header(headers, "x-hub-signature-256")
        .ok_or_else(|| unauthorized("missing X-Hub-Signature-256 header".to_string()))?;
    verify_signature(&state.secret, body, signature).map_err(|e| unauthorized(e.to_string()))?;
    let delivery_id = header(headers, "x-github-delivery")
        .ok_or_else(|| bad_request("missing X-GitHub-Delivery header".to_string()))?;
    let event_type = header(headers, "x-github-event")
        .ok_or_else(|| bad_request("missing X-GitHub-Event header".to_string()))?;
    let payload: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| bad_request(format!("body is not valid JSON: {e}")))?;
    let source_id = &state.source_id;
    if event_type == "ping" {
        debug!("[{source_id}] ping delivery {delivery_id} acknowledged");
        return Ok(None);
    }
    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let converter = Converter::new(source_id, &state.organization, effective_from)
        .with_repository_filter(&state.repository_filter);
    let changes = match converter.convert(event_type, &payload) {
        Ok(Some(changes)) => changes,
        Ok(None) => {
            debug!("[{source_id}] delivery {delivery_id} ({event_type}) has no graph change");
            return Ok(None);
        }
        Err(ConvertError::OrganizationMismatch(m)) => {
            warn!("[{source_id}] rejected delivery {delivery_id}: {m}");
            return reject(StatusCode::FORBIDDEN, m);
        }
        Err(ConvertError::InvalidPayload(m)) => {
            warn!("[{source_id}] invalid delivery {delivery_id}: {m}");
            return reject(StatusCode::UNPROCESSABLE_ENTITY, m);
        }
    };
    let key = format!("{DELIVERY_KEY_PREFIX}{delivery_id}");
    let _guard = state.gate.lock().await;
    let completed = state.state_store.contains_key(source_id, &key).await;
    if completed.map_err(|e| store_unavailable(source_id, e))? {
        debug!("[{source_id}] delivery {delivery_id} already completed; not re-appended");
        return Ok(Some(0));
    }

    // Append every change before acknowledging GitHub. A crash inside this loop
    // can leave a durable prefix, so redelivery is at-least-once by design;
    // every element ID is deterministic so repeats target the same elements.
    for change in &changes {
        match state.wal.append(source_id, change).await {
            Ok(_) => {}
            Err(WalError::CapacityExhausted(_)) => {
                let message = "source WAL capacity exhausted; redeliver later";
                return reject(StatusCode::SERVICE_UNAVAILABLE, message);
            }
            Err(e) => {
                error!("[{source_id}] WAL append failed: {e}");
                let message = format!("WAL append failed: {e}");
                return reject(StatusCode::SERVICE_UNAVAILABLE, message);
            }
        }
    }

    // Only a persisted marker makes the delivery complete; without it GitHub
    // may redeliver and the stable IDs absorb the repeat.
    state
        .state_store
        .set(source_id, &key, Vec::new())
        .await
        .map_err(|e| store_unavailable(source_id, e))?;
    state.notify.notify_one();
    Ok(Some(changes.len()))
}

fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
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
