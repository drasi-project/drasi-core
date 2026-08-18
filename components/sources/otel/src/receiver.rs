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
use log::{debug, error, info, warn};
use prost::Message;
use tokio::sync::RwLock;
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
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use crate::otlp::proto::collector::metrics::v1::{
    metrics_service_server::{MetricsService, MetricsServiceServer},
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use crate::otlp::proto::collector::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer},
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};

/// Shared runtime used by both transports and the TTL sweeper.
pub struct OtelRuntime {
    pub source_id: String,
    pub config: Arc<OtelSourceConfig>,
    pub base: SourceBase,
    pub lifecycle: Arc<RwLock<LifecycleState>>,
    pub counters: Arc<OtelCounters>,
    pub wal: Option<Arc<dyn WalProvider>>,
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
        }
    }
}

impl OtelRuntime {
    pub async fn handle_metrics(&self, request: ExportMetricsServiceRequest) -> anyhow::Result<()> {
        let now = now_millis();
        let mapped = map_metrics(&request, &self.config, now);
        self.commit(mapped, now).await
    }

    pub async fn handle_traces(&self, request: ExportTraceServiceRequest) -> anyhow::Result<()> {
        let now = now_millis();
        let mapped = map_traces(&request, &self.config, now);
        self.commit(mapped, now).await
    }

    pub async fn handle_logs(&self, request: ExportLogsServiceRequest) -> anyhow::Result<()> {
        let now = now_millis();
        let mapped = map_logs(&request, &self.config, now);
        self.commit(mapped, now).await
    }

    async fn commit(&self, mapped: crate::mapping::MapOutcome, now: u64) -> anyhow::Result<()> {
        self.counters
            .rejected
            .fetch_add(mapped.rejected, Ordering::Relaxed);
        if mapped.elements.is_empty() {
            return Ok(());
        }
        let (changes, dropped) = {
            let mut lifecycle = self.lifecycle.write().await;
            lifecycle.apply(&self.source_id, &self.config, mapped.elements)
        };
        self.counters
            .dropped
            .fetch_add(dropped as u64, Ordering::Relaxed);
        if !changes.is_empty() {
            self.counters
                .accepted
                .fetch_add(mapped.accepted.max(1), Ordering::Relaxed);
            self.emit(changes).await?;
            self.persist_lifecycle().await;
        }
        let _ = now;
        Ok(())
    }

    pub async fn expire_due(&self) -> anyhow::Result<()> {
        let now = now_millis();
        let changes = {
            let mut lifecycle = self.lifecycle.write().await;
            lifecycle.expire(&self.source_id, now)
        };
        if !changes.is_empty() {
            self.counters
                .expired
                .fetch_add(changes.len() as u64, Ordering::Relaxed);
            self.emit(changes).await?;
            self.persist_lifecycle().await;
        }
        Ok(())
    }

    async fn emit(&self, changes: Vec<SourceChange>) -> anyhow::Result<()> {
        for change in changes {
            let wal_seq = if let Some(wal) = &self.wal {
                match wal.append(&self.source_id, &change).await {
                    Ok(seq) => Some(seq),
                    Err(WalError::CapacityExhausted(msg)) => {
                        return Err(anyhow::anyhow!("WAL capacity exhausted: {msg}"));
                    }
                    Err(e) => return Err(anyhow::anyhow!("WAL append failed: {e}")),
                }
            } else {
                None
            };

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
                .context("dispatch projected OTLP change")?;
        }
        Ok(())
    }

    async fn persist_lifecycle(&self) {
        let Some(store) = self.base.state_store().await else {
            return;
        };
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

    pub async fn expected_auth(&self) -> Option<ExpectedAuth> {
        match expected_credentials(self.base.identity_provider().await, &self.config).await {
            Ok(value) => value,
            Err(e) => {
                warn!("[{}] identity provider error: {e}", self.source_id);
                None
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
        let auth = self.runtime.expected_auth().await;
        authorize_grpc(&request, auth.as_ref())?;
        self.runtime
            .handle_metrics(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ExportMetricsServiceResponse {
            partial_success: None,
        }))
    }
}

#[tonic::async_trait]
impl TraceService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let auth = self.runtime.expected_auth().await;
        authorize_grpc(&request, auth.as_ref())?;
        self.runtime
            .handle_traces(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

#[tonic::async_trait]
impl LogsService for OtlpGrpcService {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let auth = self.runtime.expected_auth().await;
        authorize_grpc(&request, auth.as_ref())?;
        self.runtime
            .handle_logs(request.into_inner())
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: None,
        }))
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
                if let Err(e) = builder
                    .add_service(MetricsServiceServer::new(svc.clone()))
                    .add_service(TraceServiceServer::new(svc.clone()))
                    .add_service(LogsServiceServer::new(svc))
                    .serve_with_incoming_shutdown(incoming, async move {
                        let _ = stop_rx.await;
                    })
                    .await
                {
                    error!("OTLP/gRPC server error: {e}");
                }
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
                if let Err(e) = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = stop_rx.await;
                    })
                    .await
                {
                    error!("OTLP/HTTP server error: {e}");
                }
            }),
        ));
    }

    let sweeper_runtime = runtime.clone();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = interval.tick() => {
                if let Err(e) = sweeper_runtime.expire_due().await {
                    debug!("[{}] TTL sweep failed: {e}", sweeper_runtime.source_id);
                }
            }
        }
    }

    if let Some((tx, handle)) = grpc_task {
        let _ = tx.send(());
        let _ = handle.await;
    }
    if let Some((tx, handle)) = http_task {
        let _ = tx.send(());
        let _ = handle.await;
    }
    Ok(())
}

async fn http_metrics(
    State(runtime): State<OtelRuntime>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    if !authorize_http(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        runtime.expected_auth().await.as_ref(),
    ) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
    if !authorize_http(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        runtime.expected_auth().await.as_ref(),
    ) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
    if !authorize_http(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        runtime.expected_auth().await.as_ref(),
    ) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let request =
        ExportLogsServiceRequest::decode(body.as_ref()).map_err(|_| StatusCode::BAD_REQUEST)?;
    runtime
        .handle_logs(request)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

fn now_millis() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}
