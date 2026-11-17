// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The SSE sink component: factory, lifecycle, management and HTTP routes.

use crate::config::{self, valid_path, SseSinkConfig};
use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::get,
    Router,
};
use drasi_computation_contracts::{
    workers::{
        join_owned_worker_gracefully, spawn_owned_worker, WorkerCleanupError, WorkerCompletion,
    },
    *,
};
use drasi_computation_network_common::{
    diag,
    security::{Authenticator, RateLimiter},
    server::{self, PeerAddress, ServerOptions},
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleError,
    LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext, StopMode,
    UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, NativeComponent, NativeManagement, NativeOutputSnapshot,
    NativeOutputSnapshots, NativeSink, UpdateRequest,
};
use drasi_reaction_common::sse::{default_changes_payload, SseRenderer};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::broadcast, sync::RwLock, task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;

/// Stop budget when the host supplies no deadline.
const DEFAULT_STOP_BUDGET: Duration = Duration::from_secs(30);
/// Seconds a refused client should wait before retrying.
const RETRY_AFTER: HeaderValue = HeaderValue::from_static("1");
const OUTPUT_EPOCH_HEADER: &str = "x-drasi-output-epoch";
const OUTPUT_POSITION_HEADER: &str = "x-drasi-output-position";
/// Query parameter accepted as a bearer token when `allowQueryToken` is set.
pub(crate) const QUERY_TOKEN: &str = "access_token";
pub(crate) const KEEP_ALIVE_COMMENT: &str = "keep-alive";

pub struct SseFactory;

impl Factory for SseFactory {
    /// Inputs do not key delivery on producer identity.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = SseSinkConfig::parse(request.request.configuration.clone())?;
        Ok(request.unkeyed_sink(config.on_permanent_failure))
    }
    fn supports_output_snapshots(&self) -> bool {
        true
    }
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(SseSinkConfig::definition().details())
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        config::assess(request)
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(crate::SSE_SINK, "1")
                .unwrap_or_else(|error| unreachable!("constant SSE implementation: {error}")),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: SseSinkConfig::definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in")
                    .unwrap_or_else(|error| unreachable!("constant port: {error}")),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Accepted),
            capabilities: Capabilities {
                binds_inputs: true,
                management: true,
                update_modes: vec![
                    ComponentUpdateMode::InPlace,
                    ComponentUpdateMode::Restart,
                    ComponentUpdateMode::Recreate,
                ],
                erases_owned_state: true,
                ..Capabilities::default()
            },
        }
    }
    fn create(&self, request: &CreateRequest, control: ControlSender) -> Result<CreatedComponent> {
        self.create_with_output_snapshots(request, control, None)
    }
    fn create_with_output_snapshots(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        snapshots: Option<NativeOutputSnapshots>,
    ) -> Result<CreatedComponent> {
        let config = SseSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        Ok(Component::Sink(Box::new(SseSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
            snapshots,
        )?))
        .into())
    }
}

/// A value shared with running request handlers and swapped atomically at a
/// Pause boundary. A handler keeps the snapshot it loaded for its request.
pub(crate) struct Live<T>(std::sync::RwLock<Arc<T>>);

impl<T> Live<T> {
    fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(Arc::new(value)))
    }
    pub(crate) fn load(&self) -> Arc<T> {
        match self.0.read() {
            Ok(value) => value.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
    fn store(&self, value: T) {
        let value = Arc::new(value);
        match self.0.write() {
            Ok(mut slot) => *slot = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }
}

/// Allowed cross-origin readers.
#[derive(Debug)]
pub(crate) enum Cors {
    Disabled,
    Any,
    Origins(Vec<String>),
}

impl Cors {
    fn from_config(origins: &[String]) -> Self {
        match origins {
            [] => Self::Disabled,
            [any] if any == "*" => Self::Any,
            origins => Self::Origins(origins.to_vec()),
        }
    }

    /// The `Access-Control-Allow-Origin` value for a request from `origin`.
    fn allow(&self, origin: Option<&HeaderValue>) -> Option<HeaderValue> {
        match self {
            Self::Disabled => None,
            Self::Any => Some(HeaderValue::from_static("*")),
            Self::Origins(allowed) => {
                let origin = origin?;
                let text = origin.to_str().ok()?;
                allowed
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(text))
                    .then(|| origin.clone())
            }
        }
    }
}

/// Settings read per request or per delivery; replaced at a Pause boundary.
pub(crate) struct Settings {
    authenticator: Authenticator,
    allow_query_token: bool,
    cors: Cors,
    rate_limiter: Option<RateLimiter>,
    max_subscribers: usize,
    heartbeat: Duration,
    keep_alive: Duration,
    event_ids: bool,
}

impl Settings {
    fn from_config(config: &SseSinkConfig) -> Result<Self> {
        let security = config.security()?;
        Ok(Self {
            authenticator: Authenticator::new(security.auth_tokens),
            allow_query_token: config.allow_query_token,
            cors: Cors::from_config(&config.cors_allowed_origins),
            rate_limiter: security.rate_limit.map(RateLimiter::new),
            max_subscribers: config.max_subscribers,
            heartbeat: config.heartbeat(),
            keep_alive: config.keep_alive(),
            event_ids: config.event_ids,
        })
    }
}

/// One broadcast notification: the per-path event counter and the body.
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub(crate) id: u64,
    pub(crate) data: Arc<str>,
}

pub(crate) struct PathEntry {
    pub(crate) sender: broadcast::Sender<Frame>,
    next_id: u64,
}

impl PathEntry {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            sender: broadcast::channel(capacity).0,
            next_id: 1,
        }
    }
}

/// The event paths of one activation: configured static paths stay for the
/// whole activation; dynamic paths appear on their first event and are pruned
/// when idle and the `maxPaths` bound is reached.
#[derive(Default)]
pub(crate) struct Paths {
    pub(crate) entries: BTreeMap<String, PathEntry>,
    statics: BTreeSet<String>,
}

impl Paths {
    fn new(statics: BTreeSet<String>, capacity: usize) -> Self {
        let mut paths = Self::default();
        paths.set_statics(statics, capacity);
        paths
    }

    /// Installs the configured static paths, keeping existing broadcasters.
    /// A formerly static path becomes an ordinary (prunable) dynamic path.
    fn set_statics(&mut self, statics: BTreeSet<String>, capacity: usize) {
        for path in &statics {
            self.entries
                .entry(path.clone())
                .or_insert_with(|| PathEntry::new(capacity));
        }
        self.statics = statics;
    }

    /// Removes dynamic paths without subscribers, except those in `keep`.
    fn prune_idle(&mut self, keep: &BTreeSet<&String>) -> usize {
        let before = self.entries.len();
        let statics = &self.statics;
        self.entries.retain(|path, entry| {
            statics.contains(path) || keep.contains(path) || entry.sender.receiver_count() > 0
        });
        before - self.entries.len()
    }
}

/// Per-activation state shared by every route.
pub(crate) struct Activation {
    /// Cancelled by Stop: ends every stream and drains the listener.
    cancel: CancellationToken,
    settings: Arc<Live<Settings>>,
    pub(crate) paths: RwLock<Paths>,
    paused: AtomicBool,
    /// Snapshot reads hold a read guard; Pause takes the write guard so it
    /// returns only once in-flight reads have ended.
    reads: Arc<RwLock<()>>,
    /// Cancelled by Pause to end in-flight snapshot reads; a child of `cancel`.
    read_cancel: std::sync::Mutex<CancellationToken>,
    subscribers: Arc<AtomicUsize>,
    outputs: Arc<SinkOutputs>,
    snapshots: Option<NativeOutputSnapshots>,
}

impl Activation {
    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    fn read_cancel(&self) -> CancellationToken {
        match self.read_cancel.lock() {
            Ok(token) => token.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn reset_read_cancel(&self) {
        let fresh = self.cancel.child_token();
        match self.read_cancel.lock() {
            Ok(mut token) => *token = fresh,
            Err(poisoned) => *poisoned.into_inner() = fresh,
        }
    }

    fn open(&self) {
        self.paused.store(false, Ordering::Release);
        self.reset_read_cancel();
    }
}

pub(crate) struct Running {
    pub(crate) activation: Arc<Activation>,
    pub(crate) server: RwLock<Option<JoinHandle<Result<()>>>>,
    pub(crate) address: SocketAddr,
}

impl Drop for Running {
    /// A safety net only: every lifecycle path stops and joins explicitly.
    fn drop(&mut self) {
        self.activation.cancel.cancel();
        if let Some(server) = self.server.get_mut() {
            server.abort();
        }
    }
}

pub(crate) struct SseSink {
    descriptor: ComponentDescriptor,
    pub(crate) config: SseSinkConfig,
    outputs: SinkOutputs,
    renderer: SseRenderer,
    settings: Arc<Live<Settings>>,
    snapshots: Option<NativeOutputSnapshots>,
    pub(crate) running: Option<Running>,
}

impl SseSink {
    pub(crate) fn new(
        descriptor: ComponentDescriptor,
        config: SseSinkConfig,
        outputs: SinkOutputs,
        snapshots: Option<NativeOutputSnapshots>,
    ) -> Result<Self> {
        Ok(Self {
            descriptor,
            renderer: config.renderer(),
            settings: Arc::new(Live::new(Settings::from_config(&config)?)),
            config,
            outputs,
            snapshots,
            running: None,
        })
    }

    /// The bound listener address while started.
    pub(crate) fn local_address(&self) -> Option<SocketAddr> {
        self.running.as_ref().map(|running| running.address)
    }
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[async_trait]
impl NativeComponent for SseSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Templates, routes, tokens and TLS material are validated at
    /// construction; there is nothing further to prepare.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }
    /// Binds the listener and starts serving. Observes cancellation and the
    /// deadline while binding; nothing is left running on failure.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if self.running.is_some() {
            return Err(anyhow::anyhow!("SSE sink is started or still needs cleanup").into());
        }
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        let security = self.config.security()?;
        let tls = security
            .tls
            .as_ref()
            .map(|tls| tls.server_config(vec![b"h2".to_vec(), b"http/1.1".to_vec()]))
            .transpose()?;
        let address = SocketAddr::new(self.config.ip()?, self.config.port);
        let listener = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => return Err(LifecycleError::cancelled()),
            _ = sleep_until(ctx.deadline()) => return Err(LifecycleError::retryable(anyhow::anyhow!(
                "SSE listener bind did not complete before the start deadline"
            ))),
            listener = TcpListener::bind(address) => listener
                .with_context(|| format!("bind native SSE listener to {address}"))?,
        };
        let address = listener
            .local_addr()
            .context("read bound SSE listener address")?;
        let cancel = CancellationToken::new();
        let activation = Arc::new(Activation {
            read_cancel: std::sync::Mutex::new(cancel.child_token()),
            cancel: cancel.clone(),
            settings: self.settings.clone(),
            paths: RwLock::new(Paths::new(
                self.renderer.static_paths(),
                self.config.broadcast_capacity,
            )),
            paused: AtomicBool::new(false),
            reads: Arc::new(RwLock::new(())),
            subscribers: Arc::new(AtomicUsize::new(0)),
            outputs: Arc::new(self.outputs.clone()),
            snapshots: self.snapshots.clone(),
        });
        let options = ServerOptions {
            tls,
            max_connections: self.config.max_connections,
            header_read_timeout: Duration::from_millis(self.config.header_read_timeout_ms),
        };
        let router = router(activation.clone());
        let component = self.descriptor.id().clone();
        diag!(
            Info,
            "SSE listener started: component={component} address={address}"
        );
        let running = self.running.insert(Running {
            activation,
            server: RwLock::new(None),
            address,
        });
        let stopped = cancel.clone();
        spawn_owned_worker(&running.server, async move {
            let result =
                server::serve(listener, router, stopped.clone().cancelled_owned(), options).await;
            if !stopped.is_cancelled() {
                diag!(
                    Error,
                    "SSE listener ended unexpectedly: component={component} result={result:?}"
                );
            }
            // Streams and handle observe the end of the listener.
            stopped.cancel();
            result
        })
        .await
        .map_err(anyhow::Error::new)?;
        Ok(())
    }
    /// Refuses new subscriptions and snapshot reads (503 with `Retry-After`),
    /// suspends heartbeats, ends in-flight snapshot reads and returns once
    /// they have settled. Connected subscribers stay connected and receive
    /// only keep-alive comments; the host delivers no input while paused.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        let running = self
            .running
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SSE sink is not running"))?;
        let activation = &running.activation;
        activation.paused.store(true, Ordering::Release);
        activation.read_cancel().cancel();
        let settled = async {
            drop(activation.reads.write().await);
        };
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancellation().cancelled() => Err(LifecycleError::cancelled()),
            _ = sleep_until(ctx.deadline()) => Err(LifecycleError::retryable(anyhow::anyhow!(
                "in-flight snapshot reads did not settle before the pause deadline"
            ))),
            _ = settled => Ok(()),
        };
        if outcome.is_err() {
            // An incomplete Pause leaves the sink serving, as before.
            activation.open();
        }
        outcome
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        let running = self
            .running
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SSE sink is not running"))?;
        running.activation.open();
        Ok(())
    }
    /// `Drain` stops accepting, ends every stream and snapshot read and joins
    /// the listener within the deadline; `Abort` also aborts the listener so
    /// connections close at once. On expiry ownership is retained and the
    /// error is retryable.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        let Some(running) = &mut self.running else {
            return Ok(());
        };
        running.activation.cancel.cancel();
        if ctx.mode() == StopMode::Abort {
            if let Some(server) = running.server.get_mut() {
                server.abort();
            }
        }
        let budget = ctx.deadline().map_or(DEFAULT_STOP_BUDGET, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        // Keep the handle on self until the join completes: a timed-out stop
        // retains cleanup responsibility for the next attempt.
        let outcome = match join_owned_worker_gracefully(running.server.get_mut(), budget).await {
            Ok(WorkerCompletion::Completed(Err(error))) => {
                diag!(
                    Warn,
                    "SSE listener had failed before stop: component={} error={error:#}",
                    self.descriptor.id()
                );
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(error @ WorkerCleanupError::TimedOut { .. }) => {
                return Err(LifecycleError::retryable(
                    anyhow::Error::new(error).context("native SSE listener is still draining"),
                ));
            }
            Err(error) => Err(anyhow::Error::new(error)
                .context("native SSE listener task failed")
                .into()),
        };
        self.running = None;
        outcome
    }
    /// The sink owns no persistent or external state: notifications are
    /// volatile. Erasing disposal therefore removes nothing and is idempotent.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        if self.running.is_some() {
            return Err(anyhow::anyhow!("dispose requires a stopped SSE sink").into());
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for SseSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        config::assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current =
            SseSinkConfig::parse(update.request.current.configuration.clone()).map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the sink configuration changed after the update was assessed",
            ));
        }
        let assessment = config::assess(&update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        let proposed =
            SseSinkConfig::parse(update.request.proposed.configuration.clone()).map_err(refused)?;
        let settings = Settings::from_config(&proposed).map_err(refused)?;
        let renderer = proposed.renderer();
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if let Some(running) = &self.running {
                    if !running.activation.is_paused() {
                        return Err(refused("in-place updates require a completed pause"));
                    }
                    running
                        .activation
                        .paths
                        .write()
                        .await
                        .set_statics(renderer.static_paths(), self.config.broadcast_capacity);
                }
            }
            UpdateBoundary::Stop => {
                if self.running.is_some() {
                    return Err(refused("restart updates require a stopped sink"));
                }
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.settings.store(settings);
        self.renderer = renderer;
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl NativeSink for SseSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let running = self.running.as_ref().context("SSE sink has not started")?;
        let activation = &running.activation;
        anyhow::ensure!(
            !activation.cancel.is_cancelled(),
            "SSE listener stopped; cleanup required"
        );
        anyhow::ensure!(
            !activation.is_paused(),
            "SSE sink is paused; input is refused until resume"
        );
        let notifications = notifications(&input, &self.outputs)?;
        let component = self.descriptor.id();
        let events = self.renderer.render_changes(
            &notifications,
            chrono::Utc::now().timestamp_millis(),
            |message| diag!(Warn, "SSE render fallback: component={component} {message}"),
        )?;
        anyhow::ensure!(
            events.iter().all(|event| valid_path(&event.path)),
            "rendered SSE path must be absolute without query, fragment or control characters and outside /snapshot"
        );
        let mut paths = activation.paths.write().await;
        let targeted: BTreeSet<&String> = events.iter().map(|event| &event.path).collect();
        let missing = |paths: &Paths| {
            targeted
                .iter()
                .filter(|path| !paths.entries.contains_key(**path))
                .count()
        };
        if paths.entries.len() + missing(&paths) > self.config.max_paths {
            let pruned = paths.prune_idle(&targeted);
            diag!(
                Debug,
                "SSE pruned idle dynamic paths: component={component} count={pruned}"
            );
        }
        anyhow::ensure!(
            paths.entries.len() + missing(&paths) <= self.config.max_paths,
            "rendered SSE paths exceed maxPaths"
        );
        // Volatile UI notifications require no listener or browser acknowledgement.
        for event in events {
            let entry = paths
                .entries
                .entry(event.path)
                .or_insert_with(|| PathEntry::new(self.config.broadcast_capacity));
            let frame = Frame {
                id: entry.next_id,
                data: Arc::from(sse_data(event.payload)),
            };
            entry.next_id += 1;
            let _ = entry.sender.send(frame);
        }
        Ok(())
    }
}

/// SSE cannot carry a carriage return inside a field, so line endings are
/// normalised to `\n` (each line becomes its own `data:` field).
fn sse_data(payload: String) -> String {
    if payload.contains('\r') {
        payload.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        payload
    }
}

/// The default browser frame an input produces without any route template.
pub fn sse_payload(input: &InputEnvelope, outputs: &SinkOutputs) -> Result<Value> {
    let notifications = notifications(input, outputs)?;
    let first = notifications
        .first()
        .context("SSE input has no graph changes")?;
    let changes = notifications
        .iter()
        .map(|notification| notification.change().to_json())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(default_changes_payload(
        first.output_json(),
        first.sequence(),
        chrono::Utc::now().timestamp_millis(),
        changes,
    ))
}

/// Decode standard graph changes from any bound output. The binding names the
/// data and no query annotation is read. Patches and identity-only deletes are
/// sent as such: the browser's row state owns applying them. An envelope
/// without changes yields none; a scheduling control or recovery frame fails.
fn notifications(input: &InputEnvelope, outputs: &SinkOutputs) -> Result<Vec<ChangeNotification>> {
    drasi_computation_network_common::wire::notifications(input, outputs)
}

// ---------------------------------------------------------------- HTTP routes

fn router(activation: Arc<Activation>) -> Router {
    Router::new()
        .route(
            "/snapshot/:alias",
            get(snapshot_route).options(preflight_route),
        )
        .fallback(events_route)
        .with_state(activation)
}

fn status(code: StatusCode, message: &'static str) -> Response {
    (code, message).into_response()
}

fn unavailable(message: &'static str) -> Response {
    let mut response = status(StatusCode::SERVICE_UNAVAILABLE, message);
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, RETRY_AFTER);
    response
}

/// Decodes `%XX` escapes; `None` for a malformed escape or invalid UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The `access_token` query parameter, if present.
pub(crate) fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == QUERY_TOKEN)
        .and_then(|(_, value)| percent_decode(value))
}

/// Access control shared by the event-stream and snapshot routes: stopped,
/// per-client rate limit (charged before authentication so failed attempts
/// are limited too), bearer token from the header or query, then pause.
fn admit(
    activation: &Activation,
    settings: &Settings,
    headers: &HeaderMap,
    query: Option<&str>,
    peer: Option<IpAddr>,
) -> Result<(), Box<Response>> {
    if activation.cancel.is_cancelled() {
        return Err(Box::new(unavailable("The SSE sink is stopping")));
    }
    if let (Some(limiter), Some(peer)) = (&settings.rate_limiter, peer) {
        if !limiter.check(peer) {
            let mut response = status(StatusCode::TOO_MANY_REQUESTS, "Client rate limit exceeded");
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, RETRY_AFTER);
            return Err(Box::new(response));
        }
    }
    if settings.authenticator.enabled() {
        let header = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let authorized = settings.authenticator.authorize(header)
            || (settings.allow_query_token
                && query_token(query).is_some_and(|token| {
                    settings
                        .authenticator
                        .authorize(Some(&format!("Bearer {token}")))
                }));
        if !authorized {
            let mut response = status(StatusCode::UNAUTHORIZED, "Missing or invalid bearer token");
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return Err(Box::new(response));
        }
    }
    if activation.is_paused() {
        return Err(Box::new(unavailable("The SSE sink is paused")));
    }
    Ok(())
}

fn apply_cors(
    response: &mut Response,
    settings: &Settings,
    origin: Option<&HeaderValue>,
    expose: bool,
) {
    let headers = response.headers_mut();
    if matches!(settings.cors, Cors::Origins(_)) {
        headers.append(header::VARY, HeaderValue::from_static("origin"));
    }
    if let Some(allowed) = settings.cors.allow(origin) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allowed);
        if expose {
            headers.insert(
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                HeaderValue::from_static("x-drasi-output-epoch, x-drasi-output-position"),
            );
        }
    }
}

/// CORS preflight. Browsers send no credentials on preflight, so it is not
/// authenticated; it is rate limited. Without an allowed origin the response
/// carries no CORS headers and the browser refuses the cross-origin read.
async fn preflight_route(State(activation): State<Arc<Activation>>, request: Request) -> Response {
    preflight(&activation, &request)
}

fn preflight(activation: &Activation, request: &Request) -> Response {
    let settings = activation.settings.load();
    if let (Some(limiter), Some(peer)) = (&settings.rate_limiter, peer_ip(request)) {
        if !limiter.check(peer) {
            return status(StatusCode::TOO_MANY_REQUESTS, "Client rate limit exceeded");
        }
    }
    let origin = request.headers().get(header::ORIGIN);
    let mut response = StatusCode::NO_CONTENT.into_response();
    apply_cors(&mut response, &settings, origin, false);
    if settings.cors.allow(origin).is_some() {
        let headers = response.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("authorization, cache-control, last-event-id"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
    }
    response
}

fn peer_ip(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<PeerAddress>()
        .map(|peer| peer.0.ip())
}

/// Every path other than `/snapshot/:alias`: GET subscribes to the event
/// stream for that exact path; OPTIONS is a CORS preflight.
async fn events_route(State(activation): State<Arc<Activation>>, request: Request) -> Response {
    if request.method() == Method::OPTIONS {
        return preflight(&activation, &request);
    }
    let settings = activation.settings.load();
    let origin = request.headers().get(header::ORIGIN).cloned();
    let mut response = if request.method() != Method::GET {
        status(StatusCode::METHOD_NOT_ALLOWED, "Only GET is supported")
    } else {
        match admit(
            &activation,
            &settings,
            request.headers(),
            request.uri().query(),
            peer_ip(&request),
        ) {
            Err(response) => *response,
            Ok(()) => subscribe(&activation, &settings, request.uri().path()).await,
        }
    };
    apply_cors(&mut response, &settings, origin.as_ref(), false);
    response
}

/// Releases one subscriber slot when its stream ends.
pub(crate) struct SubscriberSlot(Arc<AtomicUsize>);

impl SubscriberSlot {
    /// Reserves a slot under `limit`.
    pub(crate) fn acquire(count: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .ok()
            .map(|_| Self(count.clone()))
    }
}

impl Drop for SubscriberSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn subscribe(activation: &Arc<Activation>, settings: &Settings, path: &str) -> Response {
    let receiver = {
        let paths = activation.paths.read().await;
        let Some(entry) = paths.entries.get(path) else {
            return status(StatusCode::NOT_FOUND, "Unknown event path");
        };
        let Some(slot) = SubscriberSlot::acquire(&activation.subscribers, settings.max_subscribers)
        else {
            return unavailable("The SSE subscriber limit is reached");
        };
        (entry.sender.subscribe(), slot)
    };
    let (receiver, slot) = receiver;
    let stream = events(
        receiver,
        activation.clone(),
        slot,
        settings.heartbeat,
        settings.keep_alive,
    );
    let mut response = Sse::new(stream).into_response();
    // Ask buffering reverse proxies (nginx) to pass frames through at once.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// One subscriber's stream: broadcast frames, JSON heartbeats (suspended while
/// paused) and SSE comment keep-alives (always). A lagging subscriber is
/// disconnected rather than silently missing frames; Stop ends the stream.
pub(crate) fn events(
    mut receiver: broadcast::Receiver<Frame>,
    activation: Arc<Activation>,
    slot: SubscriberSlot,
    heartbeat_interval: Duration,
    keep_alive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, Infallible>> {
    async_stream::stream! {
        let _slot = slot;
        let mut heartbeat = tokio::time::interval(heartbeat_interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut keep_alive = tokio::time::interval_at(
            Instant::now() + keep_alive_interval,
            keep_alive_interval,
        );
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let event = tokio::select! {
                biased;
                _ = activation.cancel.cancelled() => break,
                message = receiver.recv() => match message {
                    Ok(frame) => {
                        let event = Event::default().data(&*frame.data);
                        if activation.settings.load().event_ids {
                            event.id(frame.id.to_string())
                        } else {
                            event
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        diag!(Warn, "SSE subscriber lagged by {count} events; closing for reconnect and snapshot");
                        break;
                    }
                },
                _ = heartbeat.tick() => {
                    if activation.is_paused() {
                        continue;
                    }
                    Event::default().data(json!({
                        "type": "heartbeat", "ts": chrono::Utc::now().timestamp_millis()
                    }).to_string())
                }
                _ = keep_alive.tick() => Event::default().comment(KEEP_ALIVE_COMMENT),
            };
            yield Ok(event);
        }
    }
}

/// The HTTP outcome of a failed snapshot read. An absent capability is explicit
/// and never an empty snapshot.
fn snapshot_failure(error: &anyhow::Error) -> (StatusCode, &'static str) {
    use drasi_computation_plugin_sdk::{abi::status, transport::Failure};
    if let Some(read) = error.downcast_ref::<OutputReadError>() {
        return match read {
            OutputReadError::Absent { .. } => (
                StatusCode::NOT_IMPLEMENTED,
                "The output does not provide a snapshot",
            ),
            OutputReadError::UnknownOutput(_) | OutputReadError::UnknownComponent(_) => {
                (StatusCode::NOT_FOUND, "Unknown output")
            }
            OutputReadError::Invalid(_) | OutputReadError::Failed(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The output snapshot failed",
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The output snapshot is not ready",
            ),
        };
    }
    match error.downcast_ref::<Failure>().map(|failure| failure.code) {
        Some(status::BUSY) => (StatusCode::TOO_MANY_REQUESTS, "Snapshot readers are busy"),
        Some(status::INVALID_ARGUMENT) => (
            StatusCode::NOT_FOUND,
            "The output is outside the snapshot capability",
        ),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The output snapshot is unavailable",
        ),
    }
}

async fn snapshot_route(
    State(activation): State<Arc<Activation>>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    let settings = activation.settings.load();
    let origin = request.headers().get(header::ORIGIN).cloned();
    let mut response = match admit(
        &activation,
        &settings,
        request.headers(),
        request.uri().query(),
        peer_ip(&request),
    ) {
        Err(response) => *response,
        Ok(()) => snapshot_response(&activation, alias).await,
    };
    apply_cors(&mut response, &settings, origin.as_ref(), true);
    response
}

async fn snapshot_response(activation: &Activation, alias: String) -> Response {
    let Some(binding) = activation.outputs.get(&alias) else {
        return status(StatusCode::NOT_FOUND, "Unknown output");
    };
    let Some(snapshots) = activation.snapshots.clone() else {
        return status(
            StatusCode::NOT_IMPLEMENTED,
            "This sink has no output snapshot capability bound",
        );
    };
    // The guard keeps a concurrent Pause waiting until this read ends.
    // Tokio's RwLock is write-preferring: a read that arrives after Pause
    // queued its write guard waits, then observes the flag below.
    let guard = activation.reads.clone().read_owned().await;
    if activation.is_paused() {
        return unavailable("The SSE sink is paused");
    }
    let cancel = activation.read_cancel();
    let endpoint = binding.endpoint();
    let schema = GraphChangeCodec::schema();
    let snapshot = tokio::select! {
        biased;
        _ = cancel.cancelled() => return unavailable("The SSE sink is pausing or stopping"),
        snapshot = snapshots.snapshot(&endpoint, &schema) => match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                diag!(Warn, "SSE snapshot for {endpoint} failed: {error:#}");
                let (code, message) = snapshot_failure(&error);
                return status(code, message);
            }
        },
    };
    let Ok(epoch) = HeaderValue::from_str(&snapshot.cursor.epoch().to_string()) else {
        return status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The output snapshot epoch is not a valid header value",
        );
    };
    let position = snapshot.cursor.sequence();
    let mut response =
        axum::body::Body::from_stream(snapshot_body(snapshot, cancel, guard)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(OUTPUT_EPOCH_HEADER, epoch);
    headers.insert(OUTPUT_POSITION_HEADER, position.into());
    response
}

/// The snapshot as a JSON array of canonical `ADD` changes, each with its real
/// identity, element shape and complete `after` image: the same change shape a
/// live frame carries, so a client applies both with one identity-keyed state.
/// Pause or Stop ends the body with an error, so the client sees a truncated
/// response rather than a falsely complete one.
fn snapshot_body(
    snapshot: NativeOutputSnapshot,
    cancel: CancellationToken,
    guard: tokio::sync::OwnedRwLockReadGuard<()>,
) -> impl futures_util::Stream<Item = std::result::Result<String, std::io::Error>> + Send {
    let endpoint = snapshot.endpoint.clone();
    let mut records = snapshot.records;
    async_stream::try_stream! {
        use futures_util::StreamExt;
        let _guard = guard;
        yield String::from("[");
        let mut ordinal = 0u64;
        loop {
            let record = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "SSE snapshot interrupted by pause or stop",
                )),
                record = records.next() => Ok(record),
            }?;
            let Some(record) = record else { break; };
            let encoded = record
                .and_then(|record| {
                    let change = GraphChange::from_operation(&ChangeOperation::Added {
                        ordinal,
                        after: record,
                    })?;
                    Ok(change.to_json()?.to_string())
                })
                .map_err(|error| {
                    diag!(Warn, "SSE snapshot stream for {endpoint} failed: {error:#}");
                    std::io::Error::other("SSE output snapshot stream failed")
                })?;
            yield if ordinal == 0 { encoded } else { format!(",{encoded}") };
            ordinal += 1;
        }
        yield String::from("]");
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A started-looking activation with no listener, for stream unit tests.
    pub(crate) fn activation(config: &SseSinkConfig) -> Result<Arc<Activation>> {
        let cancel = CancellationToken::new();
        Ok(Arc::new(Activation {
            read_cancel: std::sync::Mutex::new(cancel.child_token()),
            cancel,
            settings: Arc::new(Live::new(Settings::from_config(config)?)),
            paths: RwLock::new(Paths::default()),
            paused: AtomicBool::new(false),
            reads: Arc::new(RwLock::new(())),
            subscribers: Arc::new(AtomicUsize::new(0)),
            outputs: Arc::new(SinkOutputs::default()),
            snapshots: None,
        }))
    }

    pub(crate) fn cancel(activation: &Activation) {
        activation.cancel.cancel();
    }

    pub(crate) fn subscribers(activation: &Activation) -> &Arc<AtomicUsize> {
        &activation.subscribers
    }
}
