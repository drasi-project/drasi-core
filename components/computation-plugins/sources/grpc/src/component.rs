// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The gRPC source component: lifecycle, management and `SourceService`.

use crate::{
    config::GrpcSourceConfig,
    ingress::{Ingress, IngressError, IngressErrorKind, Live, SourceRuntime},
    proto::source,
    wire,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ComponentDescriptor, OutputEnvelope};
use drasi_computation_network_common::{
    diag,
    update::{ensure_same_component, BoundaryPlanner},
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleResult,
    PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeAdmission, NativeComponent, NativeManagement, NativeSource, Scope,
    UpdateRequest,
};
use futures_util::Stream;
use std::{net::IpAddr, pin::Pin, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore};
use tonic::{service::interceptor::InterceptedService, Request, Response, Status};

/// gRPC-specific settings read per stream; replaced at a Pause boundary.
pub(crate) struct GrpcLive {
    pub stream_idle_timeout: Duration,
}

impl GrpcLive {
    fn from_config(config: &GrpcSourceConfig) -> Self {
        Self {
            stream_idle_timeout: Duration::from_millis(config.stream_idle_timeout_ms),
        }
    }
}

pub(crate) struct GrpcSource {
    runtime: SourceRuntime,
    config: GrpcSourceConfig,
    grpc: Arc<Live<GrpcLive>>,
}

impl GrpcSource {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: GrpcSourceConfig,
        scope: Option<&Scope>,
        admission: Option<NativeAdmission>,
    ) -> Result<Self> {
        config.validate_exposure()?;
        Ok(Self {
            runtime: SourceRuntime::new(descriptor, config.ingress()?, scope, admission)?,
            grpc: Arc::new(Live::new(GrpcLive::from_config(&config))),
            config,
        })
    }
}

/// Classifies each configuration change by the weakest safe boundary.
///
/// * Pause (in place): tokens, rate limits, the unauthenticated opt-in and
///   the stream idle timeout.
/// * Stop (restart): settings bound into the tonic server at start —
///   address, port, deadlines, message size, capacities and TLS.
/// * Replace: the output stream.
pub(crate) fn plan(current: &GrpcSourceConfig, proposed: &GrpcSourceConfig) -> Assessment {
    use UpdateBoundary::*;
    let mut planner = BoundaryPlanner::default();
    let (c, p) = (current, proposed);
    planner.field("stream", c.stream != p.stream, Replace);
    planner.field("host", c.host != p.host, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field("timeoutMs", c.timeout_ms != p.timeout_ms, Stop);
    planner.field(
        "ingressCapacity",
        c.ingress_capacity != p.ingress_capacity,
        Stop,
    );
    planner.field(
        "maxMessageBytes",
        c.max_message_bytes != p.max_message_bytes,
        Stop,
    );
    planner.field(
        "maxConnections",
        c.max_connections != p.max_connections,
        Stop,
    );
    planner.field(
        "headerReadTimeoutMs",
        c.header_read_timeout_ms != p.header_read_timeout_ms,
        Stop,
    );
    planner.field(
        "maxConcurrentStreams",
        c.max_concurrent_streams != p.max_concurrent_streams,
        Stop,
    );
    planner.field(
        "tls",
        c.tls_certificate_pem != p.tls_certificate_pem
            || c.tls_private_key_pem != p.tls_private_key_pem
            || c.tls_client_ca_pem != p.tls_client_ca_pem,
        Stop,
    );
    planner.field(
        "streamIdleTimeoutMs",
        c.stream_idle_timeout_ms != p.stream_idle_timeout_ms,
        Pause,
    );
    planner.field(
        "authToken",
        c.auth_token != p.auth_token || c.auth_token_secondary != p.auth_token_secondary,
        Pause,
    );
    planner.field(
        "allowUnauthenticated",
        c.allow_unauthenticated != p.allow_unauthenticated,
        Pause,
    );
    planner.field(
        "rateLimit",
        c.rate_limit_per_second != p.rate_limit_per_second
            || c.rate_limit_burst != p.rate_limit_burst,
        Pause,
    );
    planner.assessment()
}

/// Pure planning used by the factory and the live instance.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = GrpcSourceConfig::from_value(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = GrpcSourceConfig::from_value(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    proposed.validate_exposure()?;
    Ok(plan(&current, &proposed))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

/// The tonic server's own deadline sits above the admission deadline, so an
/// admission timeout is always reported with its typed uncertainty rather
/// than a bare `DEADLINE_EXCEEDED` from the transport.
pub(crate) fn transport_timeout(admission: Duration) -> Duration {
    admission + (admission / 4).max(Duration::from_secs(1))
}

#[async_trait]
impl NativeComponent for GrpcSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.runtime.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        let ingress = self.config.ingress()?;
        // The same rustls configuration as the HTTP listener, offering HTTP/2.
        let tls = ingress
            .security
            .tls
            .as_ref()
            .map(|tls| tls.server_config(vec![b"h2".to_vec()]))
            .transpose()?;
        let options = ServeOptions {
            incoming: drasi_computation_network_common::grpc_incoming::IncomingOptions {
                tls,
                max_connections: self.config.max_connections,
                header_read_timeout: Duration::from_millis(self.config.header_read_timeout_ms),
            },
            max_concurrent_streams: self.config.max_concurrent_streams,
            timeout: transport_timeout(ingress.timeout()),
            grpc: self.grpc.clone(),
        };
        let deadline = ctx.deadline();
        self.runtime
            .start(ctx.cancellation(), deadline, move |listener, ingress| {
                serve(listener, ingress, options)
            })
            .await
    }
    /// Refuses new admissions (`UNAVAILABLE`), ends open admission streams at
    /// their next message and returns once in-flight admissions settle.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        self.runtime.pause(ctx.cancellation(), ctx.deadline()).await
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.runtime.resume()
    }
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.runtime.stop(ctx.mode(), ctx.deadline()).await
    }
    /// The source owns no persistent state; durable admission journals belong
    /// to the host's QoS channel. Erasing disposal is an idempotent no-op.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        if self.runtime.is_running() {
            return Err(anyhow::anyhow!("dispose requires a stopped source").into());
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for GrpcSource {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = GrpcSourceConfig::from_value(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed = GrpcSourceConfig::from_value(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the source configuration changed after the update was assessed",
            ));
        }
        let Some(plan) = plan(&current, &proposed).plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        proposed.validate_exposure().map_err(refused)?;
        let ingress = proposed.ingress().map_err(refused)?;
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.runtime.is_running() && !self.runtime.is_paused() {
                    return Err(refused("in-place updates require a completed pause"));
                }
                self.runtime.update_live(ingress)?;
            }
            UpdateBoundary::Stop => {
                if self.runtime.is_running() {
                    return Err(refused("restart updates require a stopped source"));
                }
                self.runtime.reconfigure(ingress, None)?;
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.grpc.store(GrpcLive::from_config(&proposed));
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for GrpcSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.runtime.next().await
    }
}

/// The gRPC status for an ingress failure.
pub(crate) fn status(error: &IngressError) -> Status {
    use IngressErrorKind::*;
    let message = error.message.clone();
    match error.kind {
        Stopped | Paused | Overloaded | Uncertain => Status::unavailable(message),
        Timeout => Status::deadline_exceeded(message),
        RateLimited => Status::resource_exhausted(message),
        Unauthorized => Status::unauthenticated(message),
        Invalid => Status::invalid_argument(message),
        Failed => Status::internal(message),
    }
}

fn admit_status(failure: crate::ingress::AdmitFailure) -> Status {
    let mut status = status(&failure.error);
    if failure.accepted > 0 {
        status = Status::new(
            status.code(),
            format!(
                "admission failed after {} events: {}",
                failure.accepted,
                status.message()
            ),
        );
    }
    status
}

/// Shared state for both gRPC services.
#[derive(Clone)]
pub(crate) struct Services {
    pub ingress: Arc<Ingress>,
    pub streams: Arc<Semaphore>,
    pub grpc: Arc<Live<GrpcLive>>,
}

impl Services {
    /// Reserves a streaming-RPC slot, failing fast when none is free.
    pub fn stream_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        if self.ingress.shutdown.is_cancelled() {
            return Err(Status::unavailable("source stopped"));
        }
        if self.ingress.is_paused() {
            return Err(status(&IngressError::paused()));
        }
        self.streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("concurrent stream limit reached"))
    }

    /// Waits for the next streamed message, bounded by the idle timeout and
    /// ended by Stop.
    pub async fn next_message<T>(
        &self,
        input: &mut tonic::Streaming<T>,
    ) -> Result<Option<T>, Status> {
        let idle = self.grpc.load().stream_idle_timeout;
        tokio::select! {
            biased;
            _ = self.ingress.shutdown.cancelled() => Err(Status::unavailable("source stopped")),
            result = tokio::time::timeout(idle, input.message()) =>
                result.map_err(|_| Status::deadline_exceeded("stream idle timeout")).and_then(|value| value),
        }
    }
}

pub(crate) fn peer<T>(request: &Request<T>) -> Option<IpAddr> {
    request.remote_addr().map(|address| address.ip())
}

/// The ingress telemetry binding entered with the request's W3C trace
/// context metadata, applied to every event the request admits.
pub(crate) fn request_scope<T>(
    ingress: &Ingress,
    request: &Request<T>,
) -> drasi_computation_plugin_sdk::telemetry::ExecutionScope {
    ingress.execution_scope.with_w3c_headers(
        request
            .metadata()
            .get("traceparent")
            .map(|value| value.as_encoded_bytes()),
        request
            .metadata()
            .get("tracestate")
            .map(|value| value.as_encoded_bytes()),
    )
}

struct SourceService(Services);
type ResponseStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl source::source_service_server::SourceService for SourceService {
    async fn submit_event(
        &self,
        request: Request<source::SubmitEventRequest>,
    ) -> std::result::Result<Response<source::SubmitEventResponse>, Status> {
        let ingress = &self.0.ingress;
        let _permit = ingress.request_permit().map_err(|error| status(&error))?;
        let scope = request_scope(ingress, &request);
        let converted = request
            .into_inner()
            .event
            .context("event is required")
            .and_then(|event| wire::grpc_change(event, &ingress.source_id));
        let change = match converted {
            Ok(change) => change,
            Err(error) => {
                diag!(Debug, "gRPC invalid event: {error:#}");
                return Ok(Response::new(source::SubmitEventResponse {
                    success: false,
                    message: "Invalid event data".into(),
                    error: error.to_string(),
                    event_id: String::new(),
                }));
            }
        };
        scope
            .scope(ingress.admit(vec![change]))
            .await
            .map_err(admit_status)?;
        Ok(Response::new(source::SubmitEventResponse {
            success: true,
            message: "Event processed successfully".into(),
            error: String::new(),
            event_id: String::new(),
        }))
    }

    type StreamEventsStream = ResponseStream<source::StreamEventResponse>;
    async fn stream_events(
        &self,
        request: Request<tonic::Streaming<source::SourceChange>>,
    ) -> std::result::Result<Response<Self::StreamEventsStream>, Status> {
        let permit = self.0.stream_permit()?;
        let services = self.0.clone();
        let peer = peer(&request);
        let scope = request_scope(&services.ingress, &request);
        let mut input = request.into_inner();
        let output = async_stream::try_stream! {
            let _permit = permit;
            let ingress = &services.ingress;
            let mut events_processed = 0u64;
            loop {
                let Some(event) = services.next_message(&mut input).await? else { break; };
                ingress.rate_limit(peer).map_err(|error| status(&error))?;
                let change = match wire::grpc_change(event, &ingress.source_id) {
                    Ok(change) => change,
                    Err(error) => {
                        diag!(Debug, "gRPC stream invalid event: {error:#}");
                        yield source::StreamEventResponse {
                            success: false, message: "Invalid event data".into(), error: error.to_string(), events_processed,
                        };
                        break;
                    }
                };
                let next = events_processed.checked_add(1)
                    .ok_or_else(|| Status::resource_exhausted("gRPC stream event count exhausted"))?;
                scope.scope(ingress.admit(vec![change])).await.map_err(admit_status)?;
                events_processed = next;
                // The published SourceService reports a cumulative accepted
                // prefix. Consumers keep the latest total; they do not sum it.
                yield source::StreamEventResponse {
                    success: true, message: "Event processed successfully".into(), error: String::new(), events_processed,
                };
            }
        };
        Ok(Response::new(Box::pin(output)))
    }

    type RequestBootstrapStream = ResponseStream<source::BootstrapResponse>;
    async fn request_bootstrap(
        &self,
        _: Request<source::BootstrapRequest>,
    ) -> std::result::Result<Response<Self::RequestBootstrapStream>, Status> {
        Err(Status::unimplemented(
            "the gRPC source has no bootstrap provider; configure a bootstrap provider on the query",
        ))
    }

    async fn health_check(
        &self,
        _: Request<()>,
    ) -> std::result::Result<Response<source::HealthCheckResponse>, Status> {
        let ingress = &self.0.ingress;
        if ingress.shutdown.is_cancelled() {
            return Err(Status::unavailable("source stopped"));
        }
        let admission = if ingress.durable().is_some() {
            "durable"
        } else {
            "volatile"
        };
        let (status, message) = if ingress.is_paused() {
            (
                source::health_check_response::Status::Degraded,
                format!("Native {admission} gRPC source is paused"),
            )
        } else {
            (
                source::health_check_response::Status::Healthy,
                format!("Native {admission} gRPC source is healthy"),
            )
        };
        Ok(Response::new(source::HealthCheckResponse {
            status: status as i32,
            message,
            version: env!("CARGO_PKG_VERSION").into(),
        }))
    }
}

pub(crate) struct ServeOptions {
    incoming: drasi_computation_network_common::grpc_incoming::IncomingOptions,
    max_concurrent_streams: usize,
    timeout: Duration,
    grpc: Arc<Live<GrpcLive>>,
}

/// Bearer-token and rate-limit check applied to every RPC on both services.
#[derive(Clone)]
struct Access(Arc<Ingress>);

impl tonic::service::Interceptor for Access {
    fn call(&mut self, request: Request<()>) -> std::result::Result<Request<()>, Status> {
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

async fn serve(listener: TcpListener, ingress: Arc<Ingress>, options: ServeOptions) -> Result<()> {
    let cancel = ingress.shutdown.clone();
    let services = Services {
        ingress: ingress.clone(),
        streams: Arc::new(Semaphore::new(options.max_concurrent_streams)),
        grpc: options.grpc,
    };
    let max_message_bytes = ingress.config.max_message_bytes;
    let source =
        source::source_service_server::SourceServiceServer::new(SourceService(services.clone()))
            .max_decoding_message_size(max_message_bytes);
    let admission = crate::proto::admission::admission_service_server::AdmissionServiceServer::new(
        crate::durable_grpc::AdmissionService(services),
    )
    .max_decoding_message_size(max_message_bytes);
    let access = Access(ingress.clone());
    tonic::transport::Server::builder()
        .timeout(options.timeout)
        .http2_keepalive_interval(Some(Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(Duration::from_secs(20)))
        .add_service(InterceptedService::new(source, access.clone()))
        .add_service(InterceptedService::new(admission, access))
        .serve_with_incoming_shutdown(
            drasi_computation_network_common::grpc_incoming::limited(listener, options.incoming),
            cancel.cancelled_owned(),
        )
        .await?;
    Ok(())
}
