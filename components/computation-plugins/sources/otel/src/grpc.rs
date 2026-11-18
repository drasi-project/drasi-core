// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! OTLP/gRPC: the `opentelemetry.proto.collector.{metrics,trace,logs}.v1`
//! export services on one tonic server.
//!
//! Every RPC passes the shared rate limit and bearer-token check, takes one of
//! `ingressCapacity` request slots, and is projected and admitted before it
//! is answered. Records the source does not project are reported through the
//! OTLP `partial_success` field; a refused admission is a gRPC error whose
//! code tells the exporter whether to retry.

use crate::engine::{Engine, ExportOutcome};
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
use crate::projection::{map_logs, map_metrics, map_traces};
use drasi_computation_network_common::grpc_incoming::{limited, IncomingOptions};
use drasi_computation_network_common::ingress::{Ingress, IngressError, IngressErrorKind};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tonic::codec::CompressionEncoding;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};

/// The gRPC status for an ingress refusal. `UNAVAILABLE` and
/// `RESOURCE_EXHAUSTED` are retryable for OTLP exporters.
pub(crate) fn status(error: &IngressError) -> Status {
    use IngressErrorKind::*;
    let message = error.message.clone();
    match error.kind {
        Stopped | Paused | Overloaded | Uncertain | Timeout => Status::unavailable(message),
        RateLimited => Status::resource_exhausted(message),
        Unauthorized => Status::unauthenticated(message),
        Invalid => Status::invalid_argument(message),
        Failed => Status::internal(message),
    }
}

/// The tonic deadline sits above the admission deadline, so a slow admission
/// is answered with its typed refusal rather than a transport timeout.
pub(crate) fn transport_timeout(admission: Duration) -> Duration {
    admission + (admission / 4).max(Duration::from_secs(1))
}

fn partial_count(outcome: ExportOutcome) -> i64 {
    i64::try_from(outcome.rejected).unwrap_or(i64::MAX)
}

#[derive(Clone)]
struct Otlp {
    ingress: Arc<Ingress>,
    engine: Arc<Engine>,
}

impl Otlp {
    async fn export(
        &self,
        project: impl FnOnce(&crate::config::Projection, u64) -> crate::projection::MapOutcome,
    ) -> Result<ExportOutcome, Status> {
        let _permit = self
            .ingress
            .request_permit()
            .map_err(|error| status(&error))?;
        self.engine
            .ingest(&self.ingress, project)
            .await
            .map_err(|error| status(&error))
    }
}

#[tonic::async_trait]
impl MetricsService for Otlp {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        let request = request.into_inner();
        let outcome =
            Otlp::export(self, |settings, now| map_metrics(&request, settings, now)).await?;
        Ok(Response::new(ExportMetricsServiceResponse {
            partial_success: (outcome.rejected > 0).then(|| ExportMetricsPartialSuccess {
                rejected_data_points: partial_count(outcome),
                error_message:
                    "some data points were not projected or were dropped at a cardinality cap"
                        .into(),
            }),
        }))
    }
}

#[tonic::async_trait]
impl TraceService for Otlp {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let request = request.into_inner();
        let outcome =
            Otlp::export(self, |settings, now| map_traces(&request, settings, now)).await?;
        Ok(Response::new(ExportTraceServiceResponse {
            partial_success: (outcome.rejected > 0).then(|| ExportTracePartialSuccess {
                rejected_spans: partial_count(outcome),
                error_message: "some spans were not projected or were dropped at a cardinality cap"
                    .into(),
            }),
        }))
    }
}

#[tonic::async_trait]
impl LogsService for Otlp {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let request = request.into_inner();
        let outcome = Otlp::export(self, |settings, now| map_logs(&request, settings, now)).await?;
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: (outcome.rejected > 0).then(|| ExportLogsPartialSuccess {
                rejected_log_records: partial_count(outcome),
                error_message:
                    "some log records were not projected or were dropped at a cardinality cap"
                        .into(),
            }),
        }))
    }
}

fn peer<T>(request: &Request<T>) -> Option<IpAddr> {
    request.remote_addr().map(|address| address.ip())
}

/// Rate limit and bearer-token check applied to every RPC.
#[derive(Clone)]
struct Access(Arc<Ingress>);

impl tonic::service::Interceptor for Access {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let authorization = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        self.0
            .authorize(peer(&request), authorization, true)
            .map_err(|error| status(&error))?;
        Ok(request)
    }
}

/// Settings bound into the gRPC server at Start.
pub(crate) struct GrpcOptions {
    pub incoming: IncomingOptions,
    pub timeout: Duration,
}

/// Serves the three OTLP export services until the activation stops.
pub(crate) async fn serve(
    listener: TcpListener,
    ingress: Arc<Ingress>,
    engine: Arc<Engine>,
    options: GrpcOptions,
) -> anyhow::Result<()> {
    let max_message_bytes = ingress.config.max_message_bytes;
    let otlp = Otlp {
        ingress: ingress.clone(),
        engine,
    };
    let access = Access(ingress.clone());
    let metrics = MetricsServiceServer::new(otlp.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(max_message_bytes);
    let traces = TraceServiceServer::new(otlp.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(max_message_bytes);
    let logs = LogsServiceServer::new(otlp)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(max_message_bytes);
    tonic::transport::Server::builder()
        .timeout(options.timeout)
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(20)))
        .add_service(InterceptedService::new(metrics, access.clone()))
        .add_service(InterceptedService::new(traces, access.clone()))
        .add_service(InterceptedService::new(logs, access))
        .serve_with_incoming_shutdown(
            limited(listener, options.incoming),
            ingress.shutdown.clone().cancelled_owned(),
        )
        .await?;
    Ok(())
}
