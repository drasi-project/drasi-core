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

use crate::config::{LeaseTrust, RepositoryFilter, TaskIssueType};
use crate::lease_ledger::LeaseLedger;
use crate::mapping::{anchor_changes, ConvertError, Converter, LifecycleScope};
use crate::worker_client::WorkerFileClient;
use crate::worker_sync::{push_touches_worker_file, WorkerSync, WorkerSyncError};
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
use log::{debug, error, info, warn};
use serde_json::json;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

type HmacSha256 = Hmac<Sha256>;

const DELIVERY_KEY_PREFIX: &str = "delivery:";
const LEDGER_KEY: &str = "leases:ledger";

pub struct IngressParams {
    pub source_id: String,
    pub organization: String,
    pub repository_filter: RepositoryFilter,
    pub task_issue_type: TaskIssueType,
    pub lease_trust: Option<LeaseTrust>,
    pub worker_client: Option<Arc<WorkerFileClient>>,
    pub path: String,
    pub secret: String,
    pub body_limit_bytes: usize,
    pub wal: Arc<dyn WalProvider>,
    pub state_store: Arc<dyn StateStoreProvider>,
    pub worker_sync: Option<Arc<WorkerSync>>,
    pub notify: Arc<Notify>,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

struct IngressState {
    source_id: String,
    organization: String,
    repository_filter: RepositoryFilter,
    task_issue_type: TaskIssueType,
    lease_trust: Option<LeaseTrust>,
    worker_client: Option<Arc<WorkerFileClient>>,
    secret: Vec<u8>,
    wal: Arc<dyn WalProvider>,
    state_store: Arc<dyn StateStoreProvider>,
    worker_sync: Option<Arc<WorkerSync>>,
    notify: Arc<Notify>,
    gate: Mutex<()>,
}

pub async fn serve(listener: TcpListener, params: IngressParams) -> Result<()> {
    let state = Arc::new(IngressState {
        source_id: params.source_id,
        organization: params.organization,
        repository_filter: params.repository_filter,
        task_issue_type: params.task_issue_type,
        lease_trust: params.lease_trust,
        worker_client: params.worker_client,
        secret: params.secret.into_bytes(),
        wal: params.wal,
        state_store: params.state_store,
        worker_sync: params.worker_sync,
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
    if event_type == "push" {
        return handle_push(state, delivery_id, &payload).await;
    }
    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let converter = Converter::new(
        source_id,
        &state.organization,
        &state.task_issue_type,
        effective_from,
    )
    .with_repository_filter(&state.repository_filter);
    let converter = match &state.lease_trust {
        Some(lease_trust) => converter.with_lease_trust(lease_trust),
        None => converter,
    };
    let conversion = match converter.convert(event_type, &payload) {
        Ok(Some(conversion)) => conversion,
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

    // The lease lifecycle only exists when it is configured. Without
    // `leaseTrust` — and therefore without the credential config validation
    // requires alongside it — nothing can ever be trusted, so a
    // lifecycle-shaped comment is simply projected with `trusted = false` and
    // no reconciliation is attempted. Trying to reach an API client that was
    // never configured would turn an ordinary untrusted comment into a 503.
    let lifecycle_configured = state.lease_trust.is_some() && state.worker_client.is_some();

    // A ledger that has never seen this task cannot tell "never acquired" from
    // "acquired before I existed" — exactly the state after a clean bootstrap,
    // where the bootstrapper's fold was transient. Such a delivery needs the
    // task's current comments from GitHub.
    //
    // That read happens *outside* the shared ingress gate: it is a remote call
    // that can take tens of seconds, and holding the gate across it would stall
    // every other delivery for this Source. The reads below are deliberately
    // dirty and only decide whether to prefetch; the authoritative checks
    // happen under the gate, and an unnecessary snapshot is discarded there.
    let mut prefetched = None;
    if let Some(scope) = conversion
        .lifecycle_scope
        .as_ref()
        .filter(|_| lifecycle_configured)
    {
        let already_completed = state
            .state_store
            .contains_key(source_id, &key)
            .await
            .unwrap_or(false);
        if !already_completed && !read_ledger(state).await?.knows_task(&scope.task_node_id) {
            prefetched = Some(fetch_task_snapshot(state, scope).await?);
        }
    }

    let _guard = state.gate.lock().await;
    let completed = state.state_store.contains_key(source_id, &key).await;
    if completed.map_err(|e| store_unavailable(source_id, e))? {
        debug!("[{source_id}] delivery {delivery_id} already completed; not re-appended");
        return Ok(Some(0));
    }

    // Fold this delivery's lease-lifecycle contributions into the durable
    // ledger and recompute every affected anchor from the artifacts that now
    // survive. Every contribution is keyed by its comment node ID and states
    // that comment's *current* contribution, so replaying a delivery converges
    // on the same ledger and the same anchors.
    let mut changes = conversion.changes;
    let mut updated_ledger = None;
    if lifecycle_configured
        && (conversion.lifecycle_scope.is_some()
            || !conversion.lifecycle.is_empty()
            || !conversion.lifecycle_anchors.is_empty())
    {
        let mut ledger = read_ledger(state).await?;
        let mut affected = BTreeSet::new();

        if let Some(scope) = &conversion.lifecycle_scope {
            // Re-check under the gate. A concurrent delivery for the same task
            // may have reconciled it while this one was fetching, in which case
            // the prefetched snapshot is stale work and is dropped. Applying it
            // anyway would also converge — `reset_task` plus the same comment
            // set is idempotent — but discarding avoids rewriting anchors
            // another delivery has already settled.
            if ledger.knows_task(&scope.task_node_id) {
                if prefetched.is_some() {
                    debug!(
                        "[{source_id}] task {} was reconciled concurrently; discarding the \
                         prefetched snapshot",
                        scope.task_node_id
                    );
                }
            } else {
                // `knows_task` only ever goes false to true inside the ingress,
                // so a prefetch is normally present here. Fetching under the
                // gate is the correctness fallback for the dirty pre-checks.
                let comments = match prefetched.take() {
                    Some(comments) => comments,
                    None => fetch_task_snapshot(state, scope).await?,
                };
                affected.extend(apply_task_snapshot(
                    state,
                    &mut ledger,
                    scope,
                    &payload,
                    comments,
                ));
            }
        }

        for intent in &conversion.lifecycle {
            affected.extend(ledger.apply(intent));
        }
        affected.extend(conversion.lifecycle_anchors.iter().cloned());
        let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
        changes.extend(anchor_changes(source_id, effective_from, &ledger, affected));
        updated_ledger = Some(ledger);
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

    // The ledger is persisted only *after* every change it implies is durable.
    // Persisting it first would let a failed append leave the ledger advanced,
    // so a redelivery would compute a smaller affected set and silently drop
    // the anchor changes the first attempt never managed to write.
    if let Some(ledger) = &updated_ledger {
        write_ledger(state, ledger).await?;
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

/// Read one task's current comments from GitHub.
///
/// This is the only remote call in the lifecycle path, and it is always made
/// outside the shared ingress gate.
async fn fetch_task_snapshot(
    state: &IngressState,
    scope: &LifecycleScope,
) -> Result<Vec<serde_json::Value>, Rejection> {
    let source_id = &state.source_id;
    let Some(client) = &state.worker_client else {
        error!("[{source_id}] lease trust is configured without an API client");
        return reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "lease lifecycle reconciliation requires the configured GitHub credential",
        );
    };
    client
        .fetch_task_comments(
            &scope.repository_owner,
            &scope.repository_name,
            scope.issue_number,
        )
        .await
        .map_err(|error| {
            error!(
                "[{source_id}] failed to reconcile task {}: {error:#}",
                scope.task_node_id
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("failed to reconcile task lifecycle; redeliver later: {error}"),
            )
        })
}

/// Rebuild one task's lease lifecycle from a fetched comment snapshot.
///
/// The comments are converted through the same `Converter` a live delivery
/// uses, reusing this delivery's own organization/repository/issue envelope so
/// exactly the same task typing, trust, and grammar rules apply. Applying the
/// same snapshot twice is idempotent.
fn apply_task_snapshot(
    state: &IngressState,
    ledger: &mut LeaseLedger,
    scope: &LifecycleScope,
    payload: &serde_json::Value,
    comments: Vec<serde_json::Value>,
) -> BTreeSet<String> {
    let source_id = &state.source_id;
    let mut affected = ledger.reset_task(&scope.task_node_id);
    let converter = lifecycle_converter(state);
    for comment in comments {
        // Reuse this delivery's own organization/repository/issue envelope so
        // the fetched comment is classified under exactly the same rules, but
        // present it as a plain observation of current state.
        let mut envelope = payload.clone();
        envelope["action"] = serde_json::json!("created");
        envelope["comment"] = comment;
        if let Some(object) = envelope.as_object_mut() {
            object.remove("changes");
            object.remove("sender");
        }
        let Ok(Some(conversion)) = converter.convert("issue_comment", &envelope) else {
            continue;
        };
        for intent in &conversion.lifecycle {
            affected.extend(ledger.apply(intent));
        }
        affected.extend(conversion.lifecycle_anchors.iter().cloned());
    }
    ledger.mark_reconciled(&scope.task_node_id);
    debug!(
        "[{source_id}] reconciled lease lifecycle for task {}",
        scope.task_node_id
    );
    affected
}

fn lifecycle_converter(state: &IngressState) -> Converter<'_> {
    let converter = Converter::new(
        &state.source_id,
        &state.organization,
        &state.task_issue_type,
        0,
    )
    .with_repository_filter(&state.repository_filter);
    match &state.lease_trust {
        Some(lease_trust) => converter.with_lease_trust(lease_trust),
        None => converter,
    }
}

/// Converge the worker graph when a `push` touched the exact configured
/// repository, ref, and path.
///
/// A push that is not about the worker file is acknowledged with no content;
/// the Source models no other repository-content state.
async fn handle_push(
    state: &IngressState,
    delivery_id: &str,
    payload: &serde_json::Value,
) -> Result<Option<usize>, Rejection> {
    let source_id = &state.source_id;
    let Some(worker_sync) = &state.worker_sync else {
        debug!("[{source_id}] push delivery {delivery_id} ignored; no worker file is configured");
        return Ok(None);
    };
    let organization = payload
        .pointer("/organization/login")
        .and_then(serde_json::Value::as_str);
    if let Some(login) = organization {
        if !login.eq_ignore_ascii_case(&state.organization) {
            let message = format!(
                "delivery organization '{login}' does not match configured organization '{}'",
                state.organization
            );
            warn!("[{source_id}] rejected delivery {delivery_id}: {message}");
            return reject(StatusCode::FORBIDDEN, message);
        }
    }
    let repository = payload
        .pointer("/repository/full_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "push delivery has no 'repository.full_name'".to_string(),
            )
        })?;
    let pushed_ref = payload
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "push delivery has no 'ref'".to_string(),
            )
        })?;
    let location = worker_sync.location();
    if !location.matches_push(repository, pushed_ref) {
        debug!(
            "[{source_id}] push delivery {delivery_id} on {repository}@{pushed_ref} is not the \
             configured worker file location"
        );
        return Ok(None);
    }
    if !push_touches_worker_file(payload, &location.path) {
        debug!(
            "[{source_id}] push delivery {delivery_id} did not touch '{}'",
            location.path
        );
        return Ok(None);
    }

    let key = format!("{DELIVERY_KEY_PREFIX}{delivery_id}");
    // Take the shared ingress gate only for the durable marker check, then
    // release it. Convergence performs a remote GitHub read that can take tens
    // of seconds; holding the gate across it would stall every other webhook
    // delivery for this Source. `WorkerSync` has its own gate, so concurrent
    // convergences still serialize against each other.
    {
        let _guard = state.gate.lock().await;
        let completed = state.state_store.contains_key(source_id, &key).await;
        if completed.map_err(|e| store_unavailable(source_id, e))? {
            debug!("[{source_id}] delivery {delivery_id} already completed; not re-appended");
            return Ok(Some(0));
        }
    }

    let outcome = match worker_sync.converge().await {
        Ok(outcome) => outcome,
        // An unreadable worker file proves nothing about configured capacity.
        // Ask GitHub to redeliver instead of asserting a worker pool state.
        Err(error @ WorkerSyncError::Unavailable(_)) => {
            error!("[{source_id}] worker file convergence failed: {error}");
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("worker file unavailable; redeliver later: {error}"),
            );
        }
        Err(error @ WorkerSyncError::Storage(_)) => {
            error!("[{source_id}] worker file convergence failed: {error}");
            return reject(StatusCode::SERVICE_UNAVAILABLE, error.to_string());
        }
    };

    // Only a persisted marker completes the delivery. Convergence is
    // idempotent on stable element IDs, so a concurrent duplicate of the same
    // delivery can at worst re-state the same worker graph.
    {
        let _guard = state.gate.lock().await;
        state
            .state_store
            .set(source_id, &key, Vec::new())
            .await
            .map_err(|e| store_unavailable(source_id, e))?;
    }
    state.notify.notify_one();
    info!(
        "[{source_id}] push delivery {delivery_id} converged the worker graph ({} change(s), \
         accepted={})",
        outcome.appended, outcome.accepted
    );
    Ok(Some(outcome.appended))
}

/// Load the durable lease ledger.
///
/// An unreadable ledger is a hard failure rather than a silent reset: rebuilding
/// it from an empty state would republish every lease as freshly acquired.
async fn read_ledger(state: &IngressState) -> Result<LeaseLedger, Rejection> {
    let source_id = &state.source_id;
    let stored = state
        .state_store
        .get(source_id, LEDGER_KEY)
        .await
        .map_err(|e| store_unavailable(source_id, e))?;
    let Some(stored) = stored else {
        return Ok(LeaseLedger::new());
    };
    serde_json::from_slice(&stored).map_err(|error| {
        error!("[{source_id}] lease ledger is unreadable: {error}");
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "lease ledger is unreadable".to_string(),
        )
    })
}

async fn write_ledger(state: &IngressState, ledger: &LeaseLedger) -> Result<(), Rejection> {
    let source_id = &state.source_id;
    let encoded = serde_json::to_vec(ledger).map_err(|error| {
        error!("[{source_id}] lease ledger cannot be encoded: {error}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "lease ledger cannot be encoded".to_string(),
        )
    })?;
    state
        .state_store
        .set(source_id, LEDGER_KEY, encoded)
        .await
        .map_err(|e| store_unavailable(source_id, e))
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
