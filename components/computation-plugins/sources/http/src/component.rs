// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The HTTP source component: lifecycle, management and HTTP routes.

use crate::{
    config::HttpSourceConfig,
    ingress::{Ingress as SharedIngress, IngressError, IngressErrorKind, Live, SourceRuntime},
    schema,
    server::{self, PeerAddress, ServerOptions},
    wire::{self, HttpSourceChange},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, OutputEnvelope, SourceSchemaReader,
};
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
use drasi_core_types::schema::SourceSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

pub(crate) type SharedSchema = Arc<Live<Option<SourceSchema>>>;

/// HTTP-specific settings read per request; replaced at a Pause boundary.
pub(crate) struct HttpLive {
    pub max_batch_events: usize,
    pub webhooks: Option<Arc<crate::webhook::Webhooks>>,
}

impl HttpLive {
    /// Builds the per-request settings, resolving webhook secrets once.
    fn resolve(config: &HttpSourceConfig) -> Result<Self> {
        Ok(Self {
            max_batch_events: config.max_batch_events,
            webhooks: config
                .webhooks
                .clone()
                .map(crate::webhook::Webhooks::new)
                .transpose()?
                .map(Arc::new),
        })
    }
}

pub(crate) struct HttpSource {
    runtime: SourceRuntime,
    config: HttpSourceConfig,
    http: Arc<Live<HttpLive>>,
    schema: SharedSchema,
}

impl HttpSource {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: HttpSourceConfig,
        scope: Option<&Scope>,
        admission: Option<NativeAdmission>,
    ) -> Result<Self> {
        config.validate_exposure(admission.is_some())?;
        Ok(Self {
            runtime: SourceRuntime::new(descriptor, config.ingress()?, scope, admission)?
                .with_adaptive(config.adaptive_settings()?)?,
            http: Arc::new(Live::new(HttpLive::resolve(&config)?)),
            schema: Arc::new(Live::new(config.webhooks.as_ref().and_then(schema::derive))),
            config,
        })
    }
}

/// Classifies each configuration change by the weakest safe boundary.
///
/// * Pause (in place): values read per request — deadlines, size and batch
///   limits, tokens, rate limits, webhook routes, mappings and authentication.
/// * Stop (restart): anything bound when the listener starts — address, port,
///   capacity, connection limits, TLS, adaptive prefetch, CORS and switching
///   webhook mode on or off.
/// * Replace: the output stream.
pub(crate) fn plan(current: &HttpSourceConfig, proposed: &HttpSourceConfig) -> Assessment {
    use UpdateBoundary::*;
    let mut planner = BoundaryPlanner::default();
    let (c, p) = (current, proposed);
    planner.field("stream", c.stream != p.stream, Replace);
    planner.field("host", c.host != p.host, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field(
        "ingressCapacity",
        c.ingress_capacity != p.ingress_capacity,
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
        "tls",
        c.tls_certificate_pem != p.tls_certificate_pem
            || c.tls_private_key_pem != p.tls_private_key_pem
            || c.tls_client_ca_pem != p.tls_client_ca_pem,
        Stop,
    );
    planner.field(
        "adaptive",
        c.adaptive_enabled != p.adaptive_enabled
            || c.adaptive_max_batch_size != p.adaptive_max_batch_size
            || c.adaptive_min_batch_size != p.adaptive_min_batch_size
            || c.adaptive_max_wait_ms != p.adaptive_max_wait_ms
            || c.adaptive_min_wait_ms != p.adaptive_min_wait_ms
            || c.adaptive_window_secs != p.adaptive_window_secs,
        Stop,
    );
    planner.field("webhookMode", c.webhook_mode() != p.webhook_mode(), Stop);
    planner.field(
        "webhooks.cors",
        c.webhooks.as_ref().map(|w| &w.cors) != p.webhooks.as_ref().map(|w| &w.cors),
        Stop,
    );
    planner.field("webhooks", c.webhooks != p.webhooks, Pause);
    planner.field("timeoutMs", c.timeout_ms != p.timeout_ms, Pause);
    planner.field(
        "maxMessageBytes",
        c.max_message_bytes != p.max_message_bytes,
        Pause,
    );
    planner.field(
        "maxBatchEvents",
        c.max_batch_events != p.max_batch_events,
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
    let current = HttpSourceConfig::from_value(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = HttpSourceConfig::from_value(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    Ok(plan(&current, &proposed))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for HttpSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.runtime.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Webhook secrets were resolved when the configuration was accepted;
    /// there is nothing further to check before binding.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        let ingress = self.config.ingress()?;
        let tls = ingress
            .security
            .tls
            .as_ref()
            .map(|tls| tls.server_config(vec![b"h2".to_vec(), b"http/1.1".to_vec()]))
            .transpose()?;
        let options = ServerOptions {
            tls,
            max_connections: self.config.max_connections,
            header_read_timeout: Duration::from_millis(self.config.header_read_timeout_ms),
        };
        let http = self.http.clone();
        let webhook_mode = self.config.webhook_mode();
        let cors = self
            .config
            .webhooks
            .as_ref()
            .and_then(|webhooks| webhooks.cors.clone())
            .filter(|cors| cors.enabled);
        let deadline = ctx.deadline();
        self.runtime
            .start(
                ctx.cancellation(),
                deadline,
                move |listener, shared| async move {
                    let ingress = Arc::new(HttpIngress { shared, http });
                    let router = router(ingress.clone(), webhook_mode, cors.as_ref())?;
                    let shutdown = ingress.shared.shutdown.clone().cancelled_owned();
                    server::serve(listener, router, shutdown, options).await
                },
            )
            .await
    }
    /// Refuses new data admissions (HTTP 503 with `Retry-After`) and returns
    /// once in-flight admissions settle. Health and producer-session metadata
    /// routes (register, status, receipt, retire) stay available.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        self.runtime.pause(ctx.cancellation(), ctx.deadline()).await
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.runtime.resume()
    }
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.runtime.stop(ctx.mode(), ctx.deadline()).await
    }
    /// The source owns no persistent state: volatile events live only in the
    /// activation, and durable admission journals belong to the host's QoS
    /// channel. Erasing disposal therefore has nothing to remove and is
    /// idempotent.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        if self.runtime.is_running() {
            return Err(anyhow::anyhow!("dispose requires a stopped source").into());
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for HttpSource {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        let assessment = assess(request)?;
        let proposed = HttpSourceConfig::from_value(request.proposed.configuration.clone())?;
        proposed.validate_exposure(self.runtime.is_durable())?;
        HttpLive::resolve(&proposed)?;
        Ok(assessment)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = HttpSourceConfig::from_value(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed = HttpSourceConfig::from_value(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the source configuration changed after the update was assessed",
            ));
        }
        let assessment = plan(&current, &proposed);
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        proposed
            .validate_exposure(self.runtime.is_durable())
            .map_err(refused)?;
        let ingress = proposed.ingress().map_err(refused)?;
        let http = HttpLive::resolve(&proposed).map_err(refused)?;
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
                self.runtime
                    .reconfigure(ingress, proposed.adaptive_settings().map_err(refused)?)?;
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.http.store(http);
        self.schema
            .store(proposed.webhooks.as_ref().and_then(schema::derive));
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for HttpSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(Arc::new(schema::WebhookSchemaReader(self.schema.clone())))
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.runtime.next().await
    }
}

/// Per-activation request state shared by every route.
pub(crate) struct HttpIngress {
    pub shared: Arc<SharedIngress>,
    http: Arc<Live<HttpLive>>,
}

impl HttpIngress {
    pub fn settings(&self) -> Arc<HttpLive> {
        self.http.load()
    }
}

#[derive(Serialize)]
pub(crate) struct EventResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub events_processed: usize,
}

pub(crate) fn status(kind: IngressErrorKind) -> StatusCode {
    use IngressErrorKind::*;
    match kind {
        Stopped | Paused | Overloaded | Timeout | Uncertain => StatusCode::SERVICE_UNAVAILABLE,
        RateLimited => StatusCode::TOO_MANY_REQUESTS,
        Unauthorized => StatusCode::UNAUTHORIZED,
        Invalid => StatusCode::BAD_REQUEST,
        Failed => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// A JSON failure body with the status and retry headers for `error`.
pub(crate) fn rejection(error: &IngressError, accepted: usize) -> Response {
    diag!(Debug, "HTTP request refused: accepted={accepted} {error}");
    let mut response = (
        status(error.kind),
        Json(EventResponse {
            success: false,
            message: format!("Admission failed after {accepted} events"),
            error: Some(error.message.clone()),
            events_processed: accepted,
        }),
    )
        .into_response();
    let headers = response.headers_mut();
    match error.kind {
        IngressErrorKind::Paused
        | IngressErrorKind::Overloaded
        | IngressErrorKind::RateLimited
        | IngressErrorKind::Stopped => {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        IngressErrorKind::Unauthorized => {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        _ => {}
    }
    response
}

pub(crate) fn invalid(message: impl std::fmt::Display) -> Response {
    rejection(&IngressError::invalid(message), 0)
}

pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, String> {
    serde_json::from_slice(body).map_err(|error| format!("invalid JSON body: {error}"))
}

async fn submit(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    match parse_json::<HttpSourceChange>(&body) {
        Ok(event) => submit_events(&ingress, source, vec![event]).await,
        Err(message) => invalid(message),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    events: Vec<HttpSourceChange>,
}

async fn batch(
    State(ingress): State<Arc<HttpIngress>>,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    let batch = match parse_json::<Batch>(&body) {
        Ok(batch) => batch,
        Err(message) => return invalid(message),
    };
    if batch.events.is_empty() || batch.events.len() > ingress.settings().max_batch_events {
        return invalid("batch is empty or exceeds maxBatchEvents");
    }
    submit_events(&ingress, source, batch.events).await
}

/// Admits standard-route events: volatile acceptance, or a durable commit
/// through the source's own server-sequenced producer.
async fn submit_events(
    ingress: &HttpIngress,
    source: String,
    events: Vec<HttpSourceChange>,
) -> Response {
    let source = match ComponentId::try_new(source) {
        Ok(id) => id,
        Err(error) => return invalid(error),
    };
    if source != ingress.shared.source_id {
        return invalid("source ID mismatch");
    }
    let events = match events
        .into_iter()
        .map(|event| wire::http_change(event, &source))
        .collect::<Result<Vec<_>>>()
    {
        Ok(events) => events,
        Err(error) => return invalid(error),
    };
    accepted_response(ingress.shared.admit(events).await, 0)
}

/// The response for a standard admission result. `skipped` counts webhook
/// mappings skipped under an accept policy.
pub(crate) fn accepted_response(
    result: Result<usize, crate::ingress::AdmitFailure>,
    skipped: usize,
) -> Response {
    match result {
        Ok(accepted) => Json(EventResponse {
            success: true,
            message: if skipped == 0 {
                format!("All {accepted} events processed successfully")
            } else {
                format!("Processed {accepted} events, {skipped} mappings skipped")
            },
            error: None,
            events_processed: accepted,
        })
        .into_response(),
        Err(failure) => rejection(&failure.error, failure.accepted),
    }
}

async fn health(State(ingress): State<Arc<HttpIngress>>) -> Response {
    let shared = &ingress.shared;
    if shared.shutdown.is_cancelled() {
        return rejection(&IngressError::stopped(), 0);
    }
    Json(serde_json::json!({
        "status": if shared.is_paused() { "paused" } else { "healthy" },
        "service": "http-source",
        "mode": if ingress.settings().webhooks.is_some() { "webhook" } else { "standard" },
        "admission": if shared.durable().is_some() { "durable" } else { "volatile" },
    }))
    .into_response()
}

fn is_length_limit(error: &axum::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if current.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        cause = current.source();
    }
    false
}

/// Access control, request slots and bounded body reading for every route
/// except `/health`. Webhook routes use their own authentication; all other
/// routes require the shared bearer token when one is configured.
async fn request_scope(
    State(ingress): State<Arc<HttpIngress>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if path == "/health" {
        return next.run(request).await;
    }
    let shared = &ingress.shared;
    let scope = shared.execution_scope.with_w3c_headers(
        request
            .headers()
            .get("traceparent")
            .map(|value| value.as_bytes()),
        request
            .headers()
            .get("tracestate")
            .map(|value| value.as_bytes()),
    );
    let webhook_route = ingress.settings().webhooks.is_some() && !path.starts_with("/sources/");
    let peer = request
        .extensions()
        .get::<PeerAddress>()
        .map(|peer| peer.0.ip());
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if let Err(error) = shared.authorize(peer, authorization, !webhook_route) {
        return rejection(&error, 0);
    }
    let _permit = match shared.request_permit() {
        Ok(permit) => permit,
        Err(error) => return rejection(&error, 0),
    };
    let live = shared.live();
    let limit = live.max_message_bytes;
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit as u64) {
        return rejection(
            &IngressError::new(
                IngressErrorKind::Invalid,
                "request body exceeds maxMessageBytes",
            ),
            0,
        )
        .map_status(StatusCode::PAYLOAD_TOO_LARGE);
    }
    // The body deadline bounds a client that stalls while sending; admission
    // applies its own deadline afterwards.
    let (parts, body) = request.into_parts();
    let bytes = tokio::select! {
        biased;
        _ = shared.shutdown.cancelled() => return rejection(&IngressError::stopped(), 0),
        bytes = tokio::time::timeout(live.timeout, axum::body::to_bytes(body, limit)) => match bytes {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) if is_length_limit(&error) => {
                return rejection(&IngressError::invalid("request body exceeds maxMessageBytes"), 0)
                    .map_status(StatusCode::PAYLOAD_TOO_LARGE);
            }
            Ok(Err(error)) => return invalid(format!("request body could not be read: {error}")),
            Err(_) => {
                return rejection(&IngressError::new(IngressErrorKind::Timeout, "request body timeout"), 0)
                    .map_status(StatusCode::REQUEST_TIMEOUT);
            }
        },
    };
    scope
        .scope(next.run(Request::from_parts(parts, axum::body::Body::from(bytes))))
        .await
}

trait MapStatus {
    fn map_status(self, status: StatusCode) -> Self;
}
impl MapStatus for Response {
    fn map_status(mut self, status: StatusCode) -> Self {
        *self.status_mut() = status;
        self
    }
}

fn router(
    ingress: Arc<HttpIngress>,
    webhook_mode: bool,
    cors: Option<&drasi_http_common::webhook::config::CorsConfig>,
) -> Result<Router> {
    let mut routes = Router::new()
        .route("/health", get(health))
        .merge(crate::durable_http::routes());
    routes = if webhook_mode {
        routes.fallback(crate::webhook::handle)
    } else {
        routes
            .route("/sources/:source/events", post(submit))
            .route("/sources/:source/events/batch", post(batch))
            .fallback(|| async { invalid("no matching route").map_status(StatusCode::NOT_FOUND) })
    };
    let routes = routes
        .layer(middleware::from_fn_with_state(
            ingress.clone(),
            request_scope,
        ))
        .with_state(ingress);
    Ok(match cors {
        Some(cors) => routes.layer(drasi_http_common::webhook::cors::build_cors_layer(cors)?),
        None => routes,
    })
}
