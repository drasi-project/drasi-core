// Copyright 2025 The Drasi Authors.
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

//! OTLP/gRPC and OTLP/HTTP receivers.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use log::{debug, info, warn};
use prost::Message;
use tokio::sync::RwLock;
use tonic::codec::CompressionEncoding;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

use drasi_core::models::SourceChange;
use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::wal::{WalError, WalProvider};

use crate::auth::{authorize_grpc, authorize_http, expected_credentials, ExpectedAuth};
use crate::config::{parse_bind, OtelSourceConfig};
use crate::counters::OtelCounters;
use crate::lifecycle::LifecycleState;
use crate::mapping::{map_logs, map_metrics, map_traces};
use crate::otlp::proto::collector::logs::v1::{
    logs_service_server::{LogsService, LogsServiceServer},
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use crate::otlp::proto::collector::metrics::v1::{
    metrics_service_server::{MetricsService, MetricsServiceServer},
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use crate::otlp::proto::collector::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer},
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};

/// Shared runtime used by both transports and the TTL sweeper.
pub struct OtelRuntime {
    pub source_id: String,
    pub config: Arc<OtelSourceConfig>,
    pub base: SourceBase,
    pub lifecycle: Arc<RwLock<LifecycleState>>,
    pub counters: Arc<OtelCounters>,
    pub wal: Option<Arc<dyn WalProvider>>,
    pub last_persist: Arc<tokio::sync::Mutex<Option<std::time::Instant>>>,
}

impl Clone for OtelRuntime {
    fn clone(&self) -> Self {
        Self {
            source_id: self.source_id.clone(),
            config: self.config.clone(),
            base: self.base.clone_shared(),
            lifecycle: self.lifecycle.clone(),
            counters: self.counters.clone(),
            wal: self.wal.clone(),
            last_persist: self.last_persist.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommitStats {
    pub rejected: u64,
    pub dropped: u64,
}

impl OtelRuntime {
    pub async fn handle_metrics(
        &self,
        request: ExportMetricsServiceRequest,
    ) -> anyhow::Result<CommitStats> {
        let now = now_millis();
        let mapped = map_metrics(&request, &self.config, now);
        self.commit(mapped, now).await
    }

    pub async fn handle_traces(
        &self,
        request: ExportTraceServiceRequest,
    ) -> anyhow::Result<CommitStats> {
        let now = now_millis();
        let mapped = map_traces(&request, &self.config, now);
        self.commit(mapped, now).await
    }

    pub async fn handle_logs(
        &self,
        request: ExportLogsServiceRequest,
    ) -> anyhow::Result<CommitStats> {
        let now = now_millis();
        let mapped = map_logs(&request, &self.config, now);
        self.commit(mapped, now).await
    }

    async fn commit(
        &self,
        mapped: crate::mapping::MapOutcome,
        now: u64,
    ) -> anyhow::Result<CommitStats> {
        self.counters
            .rejected
            .fetch_add(mapped.rejected, Ordering::Relaxed);
        let stats = CommitStats {
            rejected: mapped.rejected,
            dropped: 0,
        };
        if mapped.elements.is_empty() {
            return Ok(stats);
        }

        let mut lifecycle = self.lifecycle.write().await;
        let mut planned = lifecycle.clone();
        let (changes, dropped) = planned.apply(&self.source_id, &self.config, mapped.elements, now);
        self.counters
            .dropped
            .fetch_add(dropped as u64, Ordering::Relaxed);
        let stats = CommitStats {
            rejected: mapped.rejected,
            dropped: dropped as u64,
        };
        if changes.is_empty() {
            return Ok(stats);
        }

        let mut seqs = Vec::with_capacity(changes.len());
        for change in &changes {
            seqs.push(self.wal_append(change).await?);
        }
        *lifecycle = planned;
        drop(lifecycle);

        self.counters
            .accepted
            .fetch_add(mapped.accepted, Ordering::Relaxed);
        for (change, seq) in changes.into_iter().zip(seqs) {
            self.dispatch_change(change, seq).await?;
        }
        self.persist_lifecycle(false).await;
        Ok(stats)
    }

    pub async fn expire_due(&self) -> anyhow::Result<()> {
        let now = now_millis();
        let mut lifecycle = self.lifecycle.write().await;
        let mut planned = lifecycle.clone();
        let changes = planned.expire(&self.source_id, now);
        if changes.is_empty() {
            return Ok(());
        }
        let mut seqs = Vec::with_capacity(changes.len());
        for change in &changes {
            seqs.push(self.wal_append(change).await?);
        }
        *lifecycle = planned;
        drop(lifecycle);
        self.counters
            .expired
            .fetch_add(changes.len() as u64, Ordering::Relaxed);
        for (change, seq) in changes.into_iter().zip(seqs) {
            self.dispatch_change(change, seq).await?;
        }
        self.persist_lifecycle(true).await;
        Ok(())
    }

    async fn wal_append(&self, change: &SourceChange) -> anyhow::Result<Option<u64>> {
        let Some(wal) = &self.wal else {
            return Ok(None);
        };
        match wal.append(&self.source_id, change).await {
            Ok(seq) => Ok(Some(seq)),
            Err(WalError::CapacityExhausted(msg)) => {
                Err(anyhow::anyhow!("WAL capacity exhausted: {msg}"))
            }
            Err(e) => Err(anyhow::anyhow!("WAL append failed: {e}")),
        }
    }

    async fn dispatch_change(
        &self,
        change: SourceChange,
        wal_seq: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut wrapper = SourceEventWrapper::new(
            self.source_id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        if let Some(seq) = wal_seq {
            wrapper.sequence = Some(seq);
            wrapper.set_source_position(bytes::Bytes::copy_from_slice(&seq.to_be_bytes()));
        }
        self.base
            .dispatch_event(wrapper)
            .await
            .context("dispatch projected OTLP change")
    }

    pub async fn persist_lifecycle(&self, force: bool) {
        let Some(store) = self.base.state_store().await else {
            return;
        };
        {
            let mut last = self.last_persist.lock().await;
            let now = std::time::Instant::now();
            if !force {
                if let Some(prev) = *last {
                    if now.duration_since(prev) < Duration::from_secs(1) {
                        return;
                    }
                }
            }
            *last = Some(now);
        }
        let bytes = {
            let lifecycle = self.lifecycle.read().await;
            match lifecycle.to_bytes() {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!("[{}] failed to serialize lifecycle: {e}", self.source_id);
                    return;
                }
            }
        };
        if let Err(e) = store.set(&self.source_id, "lifecycle", bytes).await {
            warn!("[{}] failed to persist lifecycle: {e}", self.source_id);
        }
    }

    pub async fn expected_auth(&self) -> Result<Option<ExpectedAuth>, Status> {
        match expected_credentials(self.base.identity_provider().await, &self.config).await {
            Ok(value) => Ok(value),
            Err(e) => {
                warn!("[{}] identity provider error: {e}", self.source_id);
                Err(Status::unauthenticated("identity provider failed"))
            }
        }
    }
}

#[derive(Clone)]
struct OtlpGrpcService {
    runtime: OtelRuntime,
}

#[tonic::async_trait]
impl MetricsService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        let auth = self.runtime.expected_auth().await?;
        authorize_grpc(&request, auth.as_ref())?;
        let stats = self
            .runtime
            .handle_metrics(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(metrics_partial(stats)))
    }
}

#[tonic::async_trait]
impl TraceService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let auth = self.runtime.expected_auth().await?;
        authorize_grpc(&request, auth.as_ref())?;
        let stats = self
            .runtime
            .handle_traces(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(traces_partial(stats)))
    }
}

#[tonic::async_trait]
impl LogsService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let auth = self.runtime.expected_auth().await?;
        authorize_grpc(&request, auth.as_ref())?;
        let stats = self
            .runtime
            .handle_logs(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(logs_partial(stats)))
    }
}

/// Listeners bound before the source reports `Running`.
pub struct BoundEndpoints {
    pub grpc: Option<tokio::net::TcpListener>,
    pub http: Option<tokio::net::TcpListener>,
}

/// Bind OTLP sockets. Call this from `start()` before reporting Running.
pub async fn bind_endpoints(config: &OtelSourceConfig) -> anyhow::Result<BoundEndpoints> {
    let grpc = if config.grpc_bind.trim().is_empty() {
        None
    } else {
        let addr = parse_bind(&config.grpc_bind)?;
        Some(
            tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("bind OTLP/gRPC {addr}"))?,
        )
    };
    let http = match config.http_bind.as_ref().filter(|s| !s.trim().is_empty()) {
        Some(bind) => {
            let addr = parse_bind(bind)?;
            Some(
                tokio::net::TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("bind OTLP/HTTP {addr}"))?,
            )
        }
        None => None,
    };
    Ok(BoundEndpoints { grpc, http })
}

/// Run gRPC, optional HTTP, and the TTL sweeper until `shutdown` fires.
pub async fn serve(
    runtime: OtelRuntime,
    endpoints: BoundEndpoints,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let mut grpc_task = None;
    if let Some(listener) = endpoints.grpc {
        let addr = listener.local_addr().context("OTLP/gRPC local addr")?;
        let svc = OtlpGrpcService {
            runtime: runtime.clone(),
        };
        let mut builder = Server::builder();
        let max_request_bytes = runtime.config.max_request_bytes;
        if let (Some(cert), Some(key)) = (
            runtime.config.tls_cert_path.as_ref(),
            runtime.config.tls_key_path.as_ref(),
        ) {
            let cert_pem = tokio::fs::read(cert)
                .await
                .with_context(|| format!("read TLS cert {cert}"))?;
            let key_pem = tokio::fs::read(key)
                .await
                .with_context(|| format!("read TLS key {key}"))?;
            let mut tls = ServerTlsConfig::new().identity(Identity::from_pem(cert_pem, key_pem));
            if let Some(ca) = &runtime.config.tls_client_ca_path {
                let ca_pem = tokio::fs::read(ca)
                    .await
                    .with_context(|| format!("read TLS client CA {ca}"))?;
                tls = tls.client_ca_root(tonic::transport::Certificate::from_pem(ca_pem));
            }
            builder = builder.tls_config(tls).context("configure OTLP TLS")?;
        }
        info!("[{}] OTLP/gRPC listening on {addr}", runtime.source_id);
        let incoming = TcpIncoming::from_listener(listener, true, None)
            .map_err(|e| anyhow::anyhow!("OTLP/gRPC incoming: {e}"))?;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        grpc_task = Some((
            stop_tx,
            tokio::spawn(async move {
                builder
                    .add_service(
                        MetricsServiceServer::new(svc.clone())
                            .accept_compressed(CompressionEncoding::Gzip)
                            .max_decoding_message_size(max_request_bytes),
                    )
                    .add_service(
                        TraceServiceServer::new(svc.clone())
                            .accept_compressed(CompressionEncoding::Gzip)
                            .max_decoding_message_size(max_request_bytes),
                    )
                    .add_service(
                        LogsServiceServer::new(svc)
                            .accept_compressed(CompressionEncoding::Gzip)
                            .max_decoding_message_size(max_request_bytes),
                    )
                    .serve_with_incoming_shutdown(incoming, async move {
                        let _ = stop_rx.await;
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("OTLP/gRPC server error: {e}"))
            }),
        ));
    }

    let mut http_task = None;
    if let Some(listener) = endpoints.http {
        let addr = listener.local_addr().context("OTLP/HTTP local addr")?;
        let app = Router::new()
            .route("/v1/metrics", post(http_metrics))
            .route("/v1/traces", post(http_traces))
            .route("/v1/logs", post(http_logs))
            .with_state(runtime.clone());
        info!("[{}] OTLP/HTTP listening on {addr}", runtime.source_id);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        http_task = Some((
            stop_tx,
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = stop_rx.await;
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("OTLP/HTTP server error: {e}"))
            }),
        ));
    }

    let sweeper_runtime = runtime.clone();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let unexpected = loop {
        tokio::select! {
            _ = &mut shutdown => break None,
            result = wait_server_task(&mut grpc_task) => {
                break Some(result.context("OTLP/gRPC server exited unexpectedly"));
            }
            result = wait_server_task(&mut http_task) => {
                break Some(result.context("OTLP/HTTP server exited unexpectedly"));
            }
            _ = interval.tick() => {
                if let Err(e) = sweeper_runtime.expire_due().await {
                    debug!("[{}] TTL sweep failed: {e}", sweeper_runtime.source_id);
                }
            }
        }
    };

    if let Some((tx, handle)) = grpc_task.take() {
        let _ = tx.send(());
        let _ = handle.await;
    }
    if let Some((tx, handle)) = http_task.take() {
        let _ = tx.send(());
        let _ = handle.await;
    }
    match unexpected {
        Some(Err(e)) => Err(e),
        Some(Ok(())) => Err(anyhow::anyhow!(
            "OTLP listener exited while the source was still running"
        )),
        None => Ok(()),
    }
}

async fn wait_server_task(
    task: &mut Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    )>,
) -> anyhow::Result<()> {
    let Some((_, handle)) = task.as_mut() else {
        std::future::pending::<()>().await;
        return Ok(());
    };
    // Await in place so the shutdown sender is not dropped (that would stop the server).
    match handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("OTLP server task panicked: {e}")),
    }
}

async fn http_metrics(
    State(runtime): State<OtelRuntime>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    authorize_http_request(&runtime, &headers, &body).await?;
    let request =
        ExportMetricsServiceRequest::decode(body.as_ref()).map_err(|_| StatusCode::BAD_REQUEST)?;
    runtime
        .handle_metrics(request)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn http_traces(
    State(runtime): State<OtelRuntime>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    authorize_http_request(&runtime, &headers, &body).await?;
    let request =
        ExportTraceServiceRequest::decode(body.as_ref()).map_err(|_| StatusCode::BAD_REQUEST)?;
    runtime
        .handle_traces(request)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn http_logs(
    State(runtime): State<OtelRuntime>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    authorize_http_request(&runtime, &headers, &body).await?;
    let request =
        ExportLogsServiceRequest::decode(body.as_ref()).map_err(|_| StatusCode::BAD_REQUEST)?;
    runtime
        .handle_logs(request)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn authorize_http_request(
    runtime: &OtelRuntime,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(), StatusCode> {
    if body.len() > runtime.config.max_request_bytes {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let auth = runtime
        .expected_auth()
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    if !authorize_http(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        auth.as_ref(),
    ) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

fn metrics_partial(stats: CommitStats) -> ExportMetricsServiceResponse {
    let rejected = rejected_count(stats);
    if rejected == 0 {
        return ExportMetricsServiceResponse {
            partial_success: None,
        };
    }
    ExportMetricsServiceResponse {
        partial_success: Some(ExportMetricsPartialSuccess {
            rejected_data_points: rejected,
            error_message: "some metrics were rejected or dropped".to_string(),
        }),
    }
}

fn traces_partial(stats: CommitStats) -> ExportTraceServiceResponse {
    let rejected = rejected_count(stats);
    if rejected == 0 {
        return ExportTraceServiceResponse {
            partial_success: None,
        };
    }
    ExportTraceServiceResponse {
        partial_success: Some(ExportTracePartialSuccess {
            rejected_spans: rejected,
            error_message: "some spans were rejected or dropped".to_string(),
        }),
    }
}

fn logs_partial(stats: CommitStats) -> ExportLogsServiceResponse {
    let rejected = rejected_count(stats);
    if rejected == 0 {
        return ExportLogsServiceResponse {
            partial_success: None,
        };
    }
    ExportLogsServiceResponse {
        partial_success: Some(ExportLogsPartialSuccess {
            rejected_log_records: rejected,
            error_message: "some log records were rejected or dropped".to_string(),
        }),
    }
}

fn rejected_count(stats: CommitStats) -> i64 {
    stats
        .rejected
        .saturating_add(stats.dropped)
        .min(i64::MAX as u64) as i64
}

fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}
