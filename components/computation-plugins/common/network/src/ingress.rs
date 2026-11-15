// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The shared source ingress runtime used by the HTTP and gRPC sources.
//!
//! [`SourceRuntime`] owns one listener activation: it binds in Start, gates
//! admission during Pause, and settles the listener in Stop according to the
//! requested [`StopMode`]. [`Ingress`] is the per-activation handle given to the
//! transport: it performs access control, admission and pause gating.
//!
//! Admission has two modes. Volatile mode queues envelopes for `next` and
//! assigns graph sequences locally. Durable mode commits through the host's
//! QoS admission service; standard routes use the source's own server-sequenced
//! producer (see [`crate::durable`]) and the session protocol remains available.

use crate::config::IngressConfig;
use crate::security::{Authenticator, RateLimiter};
use anyhow::{Context, Result};
use drasi_computation_contracts::workers::{
    join_owned_worker_gracefully, spawn_owned_worker, WorkerCleanupError, WorkerCompletion,
};
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, GraphChangeCodec, GraphProducerIdentity,
    GraphProducerProgress, OutputEnvelope, PortId,
};
use drasi_computation_plugin_sdk::lifecycle::{
    CancellationToken as LifecycleCancellation, LifecycleError, LifecycleResult, StopMode,
};
use drasi_computation_plugin_sdk::{NativeAdmission, Scope};
use drasi_core_types::models::SourceChange;
use drasi_http_common::adaptive_batcher::{AdaptiveBatchConfig, ThroughputMonitor};
use futures_util::FutureExt;
use std::{
    collections::VecDeque,
    future::Future,
    net::{IpAddr, SocketAddr},
    panic::AssertUnwindSafe,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch, Mutex, OwnedSemaphorePermit, RwLock, RwLockReadGuard, Semaphore},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// Why the ingress refused or failed a request. Transports map each kind to
/// their own status codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressErrorKind {
    /// The activation is stopping or stopped.
    Stopped,
    /// The source is paused; retry after resume.
    Paused,
    /// A concurrency or capacity bound was reached; retry later.
    Overloaded,
    /// The admission deadline elapsed before the work was accepted.
    Timeout,
    /// The client exceeded its rate limit.
    RateLimited,
    /// Missing or invalid credentials.
    Unauthorized,
    /// The request is malformed or not valid for this source.
    Invalid,
    /// The outcome of a durable write is unknown; the client may retry, and
    /// the retry may duplicate the uncertain event.
    Uncertain,
    /// The request failed for another reason.
    Failed,
}

/// A typed ingress failure with a client-safe message.
#[derive(Clone, Debug)]
pub struct IngressError {
    pub kind: IngressErrorKind,
    pub message: String,
}

impl IngressError {
    pub fn new(kind: IngressErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn stopped() -> Self {
        Self::new(IngressErrorKind::Stopped, "source stopped")
    }
    pub fn paused() -> Self {
        Self::new(
            IngressErrorKind::Paused,
            "source paused; retry after resume",
        )
    }
    pub fn invalid(message: impl std::fmt::Display) -> Self {
        Self::new(IngressErrorKind::Invalid, message.to_string())
    }
}

impl std::fmt::Display for IngressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for IngressError {}

/// A failed batch admission and the number of leading events accepted first.
#[derive(Debug)]
pub struct AdmitFailure {
    pub accepted: usize,
    pub error: IngressError,
}

impl AdmitFailure {
    pub(crate) fn before(error: IngressError) -> Self {
        Self { accepted: 0, error }
    }
}

/// Settings read per request, replaceable at a Pause boundary without
/// rebinding the listener.
#[derive(Debug)]
pub struct LiveSettings {
    pub timeout: Duration,
    pub max_message_bytes: usize,
    pub authenticator: Authenticator,
    pub rate_limiter: Option<RateLimiter>,
}

impl LiveSettings {
    pub fn from_config(config: &IngressConfig) -> Self {
        Self {
            timeout: config.timeout(),
            max_message_bytes: config.max_message_bytes,
            authenticator: Authenticator::new(config.security.auth_tokens.clone()),
            rate_limiter: config.security.rate_limit.map(RateLimiter::new),
        }
    }
}

/// A value shared with running request handlers and swapped atomically.
#[derive(Debug)]
pub struct Live<T>(std::sync::RwLock<Arc<T>>);

impl<T> Live<T> {
    pub fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(Arc::new(value)))
    }
    /// The current value. A handler keeps the snapshot it loaded for the whole
    /// request, so a concurrent swap never mixes settings within one request.
    pub fn load(&self) -> Arc<T> {
        match self.0.read() {
            Ok(value) => value.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
    pub fn store(&self, value: T) {
        let value = Arc::new(value);
        match self.0.write() {
            Ok(mut slot) => *slot = value,
            Err(poisoned) => *poisoned.into_inner() = value,
        }
    }
}

/// Pause gating: admissions hold a read guard, Pause takes the write guard
/// after setting the flag so it returns only once in-flight admissions settle.
struct PauseState {
    paused: watch::Sender<bool>,
    admission: RwLock<()>,
}

pub struct Ingress {
    pub config: IngressConfig,
    pub source_id: ComponentId,
    pub cancel: CancellationToken,
    live: Arc<Live<LiveSettings>>,
    sender: Option<mpsc::Sender<OutputEnvelope>>,
    mode: AdmissionMode,
    requests: Arc<Semaphore>,
    pause: PauseState,
    pub(crate) implicit: Mutex<Option<crate::durable::ImplicitSession>>,
}

#[derive(Clone)]
enum AdmissionMode {
    Volatile {
        sequence: Arc<Mutex<u64>>,
        identity: GraphProducerIdentity,
    },
    Durable(NativeAdmission),
}

fn output_port() -> &'static PortId {
    static PORT: OnceLock<PortId> = OnceLock::new();
    PORT.get_or_init(|| PortId::try_new("out").unwrap_or_else(|_| unreachable!("constant port")))
}

impl Ingress {
    pub fn durable(&self) -> Option<&NativeAdmission> {
        match &self.mode {
            AdmissionMode::Durable(service) => Some(service),
            AdmissionMode::Volatile { .. } => None,
        }
    }

    /// The current per-request settings.
    pub fn live(&self) -> Arc<LiveSettings> {
        self.live.load()
    }

    pub fn is_paused(&self) -> bool {
        *self.pause.paused.borrow()
    }

    /// Per-client rate limit followed by the bearer-token check (when
    /// `token_protected`). The rate limit applies to every admitting request.
    pub fn authorize(
        &self,
        peer: Option<IpAddr>,
        authorization: Option<&str>,
        token_protected: bool,
    ) -> Result<(), IngressError> {
        let live = self.live();
        // Charge the rate budget first so failed authentication attempts are
        // limited too.
        self.rate_limit_with(&live, peer)?;
        if token_protected && !live.authenticator.authorize(authorization) {
            return Err(IngressError::new(
                IngressErrorKind::Unauthorized,
                "missing or invalid bearer token",
            ));
        }
        Ok(())
    }

    /// Charges one unit of the client's rate budget, for example per streamed message.
    pub fn rate_limit(&self, peer: Option<IpAddr>) -> Result<(), IngressError> {
        self.rate_limit_with(&self.live(), peer)
    }

    fn rate_limit_with(
        &self,
        live: &LiveSettings,
        peer: Option<IpAddr>,
    ) -> Result<(), IngressError> {
        match (&live.rate_limiter, peer) {
            (Some(limiter), Some(peer)) if !limiter.check(peer) => Err(IngressError::new(
                IngressErrorKind::RateLimited,
                "client rate limit exceeded",
            )),
            _ => Ok(()),
        }
    }

    /// Reserves one of `ingressCapacity` request slots, failing fast when the
    /// source is stopped, paused or saturated.
    pub fn request_permit(&self) -> Result<OwnedSemaphorePermit, IngressError> {
        if self.cancel.is_cancelled() {
            return Err(IngressError::stopped());
        }
        if self.is_paused() {
            return Err(IngressError::paused());
        }
        self.requests.clone().try_acquire_owned().map_err(|_| {
            IngressError::new(
                IngressErrorKind::Overloaded,
                "concurrent-request limit reached",
            )
        })
    }

    /// Enters the admission section. Rejects immediately while paused; the
    /// guard keeps a concurrent Pause waiting until this admission settles.
    pub(crate) async fn enter_admission(&self) -> Result<RwLockReadGuard<'_, ()>, IngressError> {
        if self.cancel.is_cancelled() {
            return Err(IngressError::stopped());
        }
        if self.is_paused() {
            return Err(IngressError::paused());
        }
        // Tokio's RwLock is write-preferring: a reader that arrives after
        // Pause queued its write guard waits, then observes the flag below.
        let guard = self.pause.admission.read().await;
        if self.is_paused() {
            return Err(IngressError::paused());
        }
        Ok(guard)
    }

    /// Resolves once Pause has been requested.
    pub(crate) fn pause_requested(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut paused = self.pause.paused.subscribe();
        async move {
            if paused.wait_for(|paused| *paused).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn pause_admission(
        &self,
        cancellation: &LifecycleCancellation,
        deadline: Option<Instant>,
    ) -> LifecycleResult {
        self.pause.paused.send_replace(true);
        let settled = async {
            drop(self.pause.admission.write().await);
        };
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(LifecycleError::cancelled()),
            _ = sleep_until(deadline) => Err(LifecycleError::retryable(anyhow::anyhow!(
                "in-flight admissions did not settle before the pause deadline"
            ))),
            _ = settled => Ok(()),
        };
        if outcome.is_err() {
            // An incomplete Pause leaves the source admitting, as before.
            self.pause.paused.send_replace(false);
        }
        outcome
    }

    fn resume_admission(&self) {
        self.pause.paused.send_replace(false);
    }

    /// Admits changes from a standard route: queued for `next` in volatile
    /// mode, or committed through the source's own durable producer.
    pub async fn admit(&self, changes: Vec<SourceChange>) -> Result<usize, AdmitFailure> {
        match &self.mode {
            AdmissionMode::Volatile { sequence, identity } => {
                self.admit_volatile(sequence, identity, changes).await
            }
            AdmissionMode::Durable(service) => self.admit_implicit(service, changes).await,
        }
    }

    async fn admit_volatile(
        &self,
        sequence: &Mutex<u64>,
        identity: &GraphProducerIdentity,
        changes: Vec<SourceChange>,
    ) -> Result<usize, AdmitFailure> {
        let _admission = self.enter_admission().await.map_err(AdmitFailure::before)?;
        let sender = self.sender.as_ref().ok_or_else(|| {
            AdmitFailure::before(IngressError::new(
                IngressErrorKind::Failed,
                "volatile ingress is unavailable",
            ))
        })?;
        let mut accepted = 0;
        let submit = async {
            let mut sequence = sequence.lock().await;
            for change in changes {
                let permit = sender
                    .reserve()
                    .await
                    .map_err(|_| IngressError::stopped())?;
                let next = sequence.checked_add(1).ok_or_else(|| {
                    IngressError::new(IngressErrorKind::Failed, "ingress sequence exhausted")
                })?;
                let mut envelope = GraphChangeCodec::encode_change(
                    change,
                    self.config.stream.clone(),
                    next,
                    Some(chrono::Utc::now()),
                )
                .map_err(IngressError::invalid)?;
                GraphProducerProgress::annotate(&mut envelope, identity, next).map_err(
                    |error| IngressError::new(IngressErrorKind::Failed, error.to_string()),
                )?;
                // No await between sequence assignment and enqueue: cancellation
                // while waiting for capacity cannot consume an event sequence.
                *sequence = next;
                permit.send(OutputEnvelope {
                    port: output_port().clone(),
                    envelope,
                });
                accepted += 1;
            }
            Ok::<(), IngressError>(())
        };
        let timeout = self.live().timeout;
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(IngressError::stopped()),
            _ = self.pause_requested() => Err(IngressError::paused()),
            result = tokio::time::timeout(timeout, submit) => result
                .unwrap_or_else(|_| Err(IngressError::new(IngressErrorKind::Timeout, "ingress capacity/admission timeout"))),
        };
        result
            .map(|()| accepted)
            .map_err(|error| AdmitFailure { accepted, error })
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

struct Running {
    cancel: CancellationToken,
    ingress: Arc<Ingress>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    listener: RwLock<Option<JoinHandle<Result<()>>>>,
    adaptive: Option<AdaptiveRead>,
    address: SocketAddr,
}

struct AdaptiveRead {
    config: AdaptiveBatchConfig,
    monitor: ThroughputMonitor,
    pending: VecDeque<OutputEnvelope>,
    deadline: Option<Instant>,
    target: usize,
}

async fn receive_volatile(
    receiver: &mut mpsc::Receiver<OutputEnvelope>,
    adaptive: &mut Option<AdaptiveRead>,
) -> Option<OutputEnvelope> {
    let Some(batch) = adaptive else {
        return receiver.recv().await;
    };
    let deadline = match batch.deadline {
        Some(deadline) => deadline,
        None => {
            if let Some(ready) = batch.pending.pop_front() {
                return Some(ready);
            }
            batch.pending.push_back(receiver.recv().await?);
            let (target, wait) = batch
                .config
                .parameters(batch.monitor.get_throughput_level());
            batch.target = target;
            let deadline = Instant::now() + wait;
            batch.deadline = Some(deadline);
            deadline
        }
    };
    while batch.pending.len() < batch.target {
        match tokio::time::timeout_at(deadline, receiver.recv()).await {
            Ok(Some(envelope)) => batch.pending.push_back(envelope),
            Ok(None) | Err(_) => break,
        }
    }
    batch.monitor.record_batch(batch.pending.len());
    batch.deadline = None;
    batch.pending.pop_front()
}

impl Running {
    /// Joins the listener, retaining ownership on timeout or cancellation.
    async fn join(&mut self, budget: Duration) -> LifecycleResult {
        match join_owned_worker_gracefully(self.listener.get_mut(), budget).await {
            Ok(WorkerCompletion::Completed(result)) => {
                result.context("native listener failed")?;
                Ok(())
            }
            Ok(WorkerCompletion::Absent | WorkerCompletion::Cancelled) => Ok(()),
            Err(error @ WorkerCleanupError::TimedOut { .. }) => Err(LifecycleError::retryable(
                anyhow::Error::new(error).context("native listener is still draining"),
            )),
            Err(error) => Err(anyhow::Error::new(error)
                .context("native listener task failed")
                .into()),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(receiver) = &mut self.receiver {
            receiver.close();
        }
        if let Some(listener) = self.listener.get_mut() {
            listener.abort();
        }
    }
}

/// Owns one source's listener configuration and its current activation.
pub struct SourceRuntime {
    pub descriptor: ComponentDescriptor,
    config: IngressConfig,
    live: Arc<Live<LiveSettings>>,
    mode: AdmissionMode,
    running: Option<Running>,
    adaptive: Option<AdaptiveBatchConfig>,
}

impl SourceRuntime {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: IngressConfig,
        scope: Option<&Scope>,
        admission: Option<NativeAdmission>,
    ) -> Result<Self> {
        config.validate()?;
        let mode = match admission {
            Some(service) => {
                anyhow::ensure!(
                    service.identity().stream() == &config.stream
                        && service.identity().component_id() == descriptor.id(),
                    "native source identity or stream does not match its admission channel"
                );
                AdmissionMode::Durable(service)
            }
            None => AdmissionMode::Volatile {
                sequence: Arc::new(Mutex::new(0)),
                identity: GraphProducerIdentity::volatile(
                    scope
                        .map_or("native-standalone", |scope| scope.instance_id.as_str())
                        .into(),
                    scope
                        .map_or("native-standalone", |scope| scope.graph_id.as_str())
                        .into(),
                    descriptor.id().clone(),
                    config.stream.clone(),
                )?,
            },
        };
        Ok(Self {
            descriptor,
            live: Arc::new(Live::new(LiveSettings::from_config(&config))),
            config,
            mode,
            running: None,
            adaptive: None,
        })
    }

    pub fn with_adaptive(mut self, config: Option<AdaptiveBatchConfig>) -> Result<Self> {
        self.set_adaptive(config)?;
        Ok(self)
    }

    fn set_adaptive(&mut self, config: Option<AdaptiveBatchConfig>) -> Result<()> {
        anyhow::ensure!(
            config.is_none() || matches!(self.mode, AdmissionMode::Volatile { .. }),
            "adaptive prefetch is a volatile ingress option; durable admission is scheduled by the host"
        );
        if let Some(config) = &config {
            validate_adaptive(config, self.config.ingress_capacity)?;
        }
        self.adaptive = config;
        Ok(())
    }

    pub fn config(&self) -> &IngressConfig {
        &self.config
    }

    pub fn is_durable(&self) -> bool {
        matches!(self.mode, AdmissionMode::Durable(_))
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The bound listener address while running; useful when `port` is zero.
    pub fn local_address(&self) -> Option<SocketAddr> {
        self.running.as_ref().map(|running| running.address)
    }

    /// The live per-request settings shared with running handlers.
    pub fn live(&self) -> Arc<Live<LiveSettings>> {
        self.live.clone()
    }

    /// Replaces the listener configuration for the next Start. The stream and
    /// admission binding are fixed for the object's lifetime.
    pub fn reconfigure(
        &mut self,
        config: IngressConfig,
        adaptive: Option<AdaptiveBatchConfig>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.running.is_none(),
            "listener configuration changes require a stopped source"
        );
        anyhow::ensure!(
            config.stream == self.config.stream,
            "the source stream is fixed for the component's lifetime"
        );
        config.validate()?;
        if let Some(adaptive) = &adaptive {
            validate_adaptive(adaptive, config.ingress_capacity)?;
        }
        anyhow::ensure!(
            adaptive.is_none() || matches!(self.mode, AdmissionMode::Volatile { .. }),
            "adaptive prefetch is a volatile ingress option"
        );
        self.live.store(LiveSettings::from_config(&config));
        self.config = config;
        self.adaptive = adaptive;
        Ok(())
    }

    /// Replaces the settings read per request; running handlers observe the
    /// new values on their next request.
    pub fn update_live(&mut self, config: IngressConfig) -> Result<()> {
        config.validate()?;
        self.live.store(LiveSettings::from_config(&config));
        self.config = config;
        Ok(())
    }

    /// Binds the listener and starts serving. Observes cancellation and the
    /// deadline while binding; nothing is left running on failure.
    pub async fn start<F, Work>(
        &mut self,
        cancellation: &LifecycleCancellation,
        deadline: Option<Instant>,
        serve: F,
    ) -> LifecycleResult
    where
        F: FnOnce(TcpListener, Arc<Ingress>) -> Work + Send,
        Work: Future<Output = Result<()>> + Send + 'static,
    {
        if self.running.is_some() {
            return Err(
                anyhow::anyhow!("native source is already started or still needs cleanup").into(),
            );
        }
        let address = SocketAddr::new(self.config.ip()?, self.config.port);
        let listener = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(LifecycleError::cancelled()),
            _ = sleep_until(deadline) => return Err(LifecycleError::retryable(anyhow::anyhow!("listener bind did not complete before the start deadline"))),
            listener = TcpListener::bind(address) => listener
                .with_context(|| format!("bind native source to {address}"))?,
        };
        let address = listener
            .local_addr()
            .context("read bound listener address")?;
        crate::diag!(
            Info,
            "listener started: component={} address={address}",
            self.descriptor.id()
        );
        let (sender, receiver) = match &self.mode {
            AdmissionMode::Volatile { .. } => {
                let (sender, receiver) = mpsc::channel(self.config.ingress_capacity);
                (Some(sender), Some(receiver))
            }
            AdmissionMode::Durable(_) => (None, None),
        };
        let cancel = CancellationToken::new();
        let ingress = Arc::new(Ingress {
            config: self.config.clone(),
            source_id: self.descriptor.id().clone(),
            sender,
            cancel: cancel.clone(),
            live: self.live.clone(),
            mode: self.mode.clone(),
            requests: Arc::new(Semaphore::new(self.config.ingress_capacity)),
            pause: PauseState {
                paused: watch::channel(false).0,
                admission: RwLock::new(()),
            },
            implicit: Mutex::new(None),
        });
        let stopped = cancel.clone();
        let admission = ingress.durable().cloned();
        let component = self.descriptor.id().clone();
        let work = serve(listener, ingress.clone());
        let running = self.running.insert(Running {
            cancel,
            ingress,
            receiver,
            listener: RwLock::new(None),
            adaptive: self.adaptive.clone().map(|config| AdaptiveRead {
                monitor: ThroughputMonitor::new(config.throughput_window),
                config,
                pending: VecDeque::new(),
                deadline: None,
                target: 1,
            }),
            address,
        });
        spawn_owned_worker(&running.listener, async move {
            let _cancel = stopped.clone().drop_guard();
            let result = AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .map_err(|panic| {
                    anyhow::anyhow!(
                        "native listener panicked: {}",
                        panic
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| panic.downcast_ref::<&str>().copied())
                            .unwrap_or("non-string panic payload")
                    )
                })
                .and_then(|result| result);
            if !stopped.is_cancelled() {
                let mut reason = match &result {
                    Ok(()) => "native listener ended unexpectedly".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                crate::diag!(
                    Error,
                    "listener failed: component={component} reason={reason}"
                );
                if let Some(admission) = admission {
                    truncate(&mut reason, 4096);
                    if let Err(error) = admission.report_failure(&reason) {
                        crate::diag!(Error, "listener failure could not reach graph: {error}");
                    }
                }
            }
            result
        })
        .await
        .map_err(anyhow::Error::new)?;
        Ok(())
    }

    /// Completes a Pause: new admissions are refused and the call returns once
    /// in-flight admissions settle. Volatile events already queued remain
    /// queued for `next`. An incomplete Pause leaves the source admitting.
    pub async fn pause(
        &self,
        cancellation: &LifecycleCancellation,
        deadline: Option<Instant>,
    ) -> LifecycleResult {
        let running = self
            .running
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native source is not running"))?;
        running
            .ingress
            .pause_admission(cancellation, deadline)
            .await
    }

    pub fn resume(&self) -> LifecycleResult {
        let running = self
            .running
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native source is not running"))?;
        running.ingress.resume_admission();
        Ok(())
    }

    pub fn is_paused(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| running.ingress.is_paused())
    }

    pub async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let running = self
            .running
            .as_mut()
            .context("native source has not started")?;
        let receiver = running
            .receiver
            .as_mut()
            .context("durable source is driven by graph admission, not next")?;
        tokio::select! {
            biased;
            _ = running.cancel.cancelled() => {
                running.join(Duration::MAX).await.map_err(LifecycleError::into_source)?;
                anyhow::bail!("native source listener closed");
            }
            envelope = receive_volatile(receiver, &mut running.adaptive) => match envelope {
                Some(envelope) => Ok(Some(envelope)),
                None => {
                    running.join(Duration::MAX).await.map_err(LifecycleError::into_source)?;
                    anyhow::bail!("native ingress closed unexpectedly");
                }
            },
        }
    }

    /// Settles the activation. `Drain` stops accepting connections and lets
    /// in-flight requests finish; `Abort` also aborts the listener and its
    /// connections. The listener is joined within the deadline (or the
    /// configured timeout); on expiry ownership is retained for a retry.
    pub async fn stop(&mut self, mode: StopMode, deadline: Option<Instant>) -> LifecycleResult {
        let Some(running) = &mut self.running else {
            return Ok(());
        };
        running.cancel.cancel();
        running.ingress.resume_admission();
        if let Some(receiver) = &mut running.receiver {
            receiver.close();
        }
        if mode == StopMode::Abort {
            if let Some(listener) = running.listener.get_mut() {
                listener.abort();
            }
        }
        let budget = deadline.map_or_else(
            || self.config.timeout() + Duration::from_secs(5),
            |deadline| deadline.saturating_duration_since(Instant::now()),
        );
        // Keep the handle on self until the join completes: a cancelled or
        // timed-out stop retains cleanup responsibility for the next attempt.
        running.join(budget).await?;
        let mut discarded = running.adaptive.as_mut().map_or(0, |batch| {
            let count = batch.pending.len();
            batch.pending.clear();
            count
        });
        if let Some(receiver) = &mut running.receiver {
            while receiver.try_recv().is_ok() {
                discarded += 1;
            }
        }
        if discarded > 0 {
            crate::diag!(
                Warn,
                "volatile ingress stopped with acknowledged but unprocessed events: component={} discarded={discarded}",
                self.descriptor.id()
            );
        }
        self.running = None;
        Ok(())
    }
}

fn validate_adaptive(config: &AdaptiveBatchConfig, ingress_capacity: usize) -> Result<()> {
    anyhow::ensure!(
        config.min_batch_size > 0
            && config.min_batch_size <= config.max_batch_size
            && config.max_batch_size <= ingress_capacity
            && !config.min_wait_time.is_zero()
            && config.min_wait_time <= config.max_wait_time
            && config.max_wait_time <= Duration::from_secs(60)
            && !config.throughput_window.is_zero()
            && config.throughput_window <= Duration::from_secs(3600),
        "invalid bounded adaptive ingress settings"
    );
    Ok(())
}

fn truncate(reason: &mut String, limit: usize) {
    if reason.len() > limit {
        let mut end = limit - 3;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        reason.push_str("...");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IngressSecurity;
    use drasi_core_types::models::{Element, ElementMetadata, ElementReference};
    use tokio::sync::Notify;

    const SOURCE_ID: &str = "495412a1-7a8f-505f-a9ca-a0da3eeadc36";
    const OTHER_SOURCE_ID: &str = "9a7efebd-4820-5bea-9eb3-17e12e60b60a";

    fn descriptor() -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            ComponentId::try_new(SOURCE_ID)?,
            vec![],
        )?)
    }

    fn config(capacity: usize) -> Result<IngressConfig> {
        Ok(IngressConfig {
            stream: drasi_computation_contracts::StreamId::try_new("test")?,
            host: "127.0.0.1".into(),
            port: 0,
            timeout_ms: 60_000,
            ingress_capacity: capacity,
            max_message_bytes: 2 * 1024 * 1024,
            max_connections: 16,
            security: IngressSecurity::default(),
        })
    }

    fn runtime() -> Result<SourceRuntime> {
        SourceRuntime::new(descriptor()?, config(1024)?, None, None)
    }

    fn token() -> LifecycleCancellation {
        LifecycleCancellation::new()
    }

    fn change(id: &str) -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", id),
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from: 1,
                },
                properties: Default::default(),
            },
        }
    }

    /// Starts a runtime whose listener only exposes its ingress to the test.
    async fn started(runtime: &mut SourceRuntime) -> Result<Arc<Ingress>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        runtime
            .start(&token(), None, move |_listener, ingress| async move {
                let cancel = ingress.cancel.clone();
                let _ = sender.send(ingress);
                cancel.cancelled().await;
                Ok(())
            })
            .await
            .map_err(LifecycleError::into_source)?;
        Ok(receiver.await?)
    }

    #[test]
    fn source_identity_is_bound_only_from_the_descriptor() -> Result<()> {
        let runtime = SourceRuntime::new(descriptor()?, config(8)?, None, None)?;
        assert_eq!(runtime.descriptor.id(), &ComponentId::try_new(SOURCE_ID)?);
        let other = ComponentId::try_new(OTHER_SOURCE_ID)?;
        let runtime = SourceRuntime::new(
            ComponentDescriptor::try_new(other.clone(), vec![])?,
            config(8)?,
            None,
            None,
        )?;
        assert_eq!(runtime.descriptor.id(), &other);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_or_panicked_listeners_require_cleanup_but_do_not_poison_retries() -> Result<()>
    {
        for panics in [false, true] {
            let mut runtime = runtime()?;
            runtime
                .start(&token(), None, move |_listener, _ingress| async move {
                    if panics {
                        panic!("listener panic probe");
                    }
                    anyhow::bail!("listener error probe");
                })
                .await
                .map_err(LifecycleError::into_source)?;
            runtime
                .running
                .as_ref()
                .expect("running owner")
                .cancel
                .cancelled()
                .await;
            let error = runtime
                .stop(StopMode::Drain, None)
                .await
                .expect_err("listener failure");
            assert!(
                format!("{:#}", error.source_error()).contains(if panics {
                    "listener panic probe"
                } else {
                    "listener error probe"
                }),
                "{error:#}"
            );
            assert!(
                runtime
                    .start(&token(), None, |_, _| async { Ok(()) })
                    .await
                    .is_err(),
                "failed cleanup still owns the run"
            );
            runtime.stop(StopMode::Drain, None).await?;
            started(&mut runtime).await?;
            runtime.stop(StopMode::Drain, None).await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_stop_retains_listener_until_join_and_unexpected_exit_is_not_eof(
    ) -> Result<()> {
        let mut runtime = runtime()?;
        let entered = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let worker_entered = entered.clone();
        let worker_resume = resume.clone();
        runtime
            .start(&token(), None, move |_listener, ingress| async move {
                ingress.cancel.cancelled().await;
                worker_entered.notify_one();
                worker_resume.notified().await;
                Ok(())
            })
            .await?;
        tokio::select! {
            result = runtime.stop(StopMode::Drain, None) => panic!("stop completed before listener exit: {result:?}"),
            _ = entered.notified() => {}
        }
        let listener = runtime
            .running
            .as_mut()
            .expect("cancelled stop retains owner")
            .listener
            .get_mut()
            .as_ref()
            .expect("cancelled stop retains join handle");
        assert!(!listener.is_finished());
        assert!(runtime
            .start(&token(), None, |_, _| async { Ok(()) })
            .await
            .is_err());
        resume.notify_one();
        runtime.stop(StopMode::Drain, None).await?;
        runtime
            .start(&token(), None, |_, _| async { Ok(()) })
            .await?;
        assert!(
            runtime.next().await.is_err(),
            "listener exit must not be successful EOF"
        );
        runtime.stop(StopMode::Drain, None).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_deadline_retains_a_draining_listener_and_abort_cancels_it() -> Result<()> {
        let mut runtime = runtime()?;
        runtime
            .start(&token(), None, |_listener, _ingress| async move {
                // Simulates connections that never finish draining.
                std::future::pending::<()>().await;
                Ok(())
            })
            .await?;
        let error = runtime
            .stop(
                StopMode::Drain,
                Some(Instant::now() + Duration::from_millis(20)),
            )
            .await
            .expect_err("drain cannot finish");
        assert!(error.is_retryable(), "{error:#}");
        assert!(runtime.is_running(), "timed-out drain retains ownership");
        runtime.stop(StopMode::Abort, None).await?;
        assert!(!runtime.is_running());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_cancelled_start_binds_nothing() -> Result<()> {
        let mut runtime = runtime()?;
        let cancelled = token();
        cancelled.cancel();
        let error = runtime
            .start(&cancelled, None, |_, _| async { Ok(()) })
            .await
            .expect_err("cancelled start");
        assert_eq!(
            error.kind(),
            drasi_computation_plugin_sdk::lifecycle::LifecycleErrorKind::Cancelled
        );
        assert!(!runtime.is_running());
        started(&mut runtime).await?;
        runtime.stop(StopMode::Drain, None).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pause_refuses_new_admission_keeps_queued_events_and_resume_reopens() -> Result<()> {
        let mut runtime = runtime()?;
        let ingress = started(&mut runtime).await?;
        assert_eq!(ingress.admit(vec![change("a")]).await.expect("admit"), 1);
        runtime.pause(&token(), None).await?;
        assert!(runtime.is_paused());
        let failure = ingress
            .admit(vec![change("b")])
            .await
            .expect_err("paused admission");
        assert_eq!(failure.error.kind, IngressErrorKind::Paused);
        assert_eq!(failure.accepted, 0);
        assert_eq!(
            ingress.request_permit().expect_err("paused permit").kind,
            IngressErrorKind::Paused
        );
        // The event accepted before Pause is still delivered.
        assert!(runtime.next().await?.is_some());
        runtime.resume()?;
        assert_eq!(ingress.admit(vec![change("c")]).await.expect("admit"), 1);
        assert!(runtime.next().await?.is_some());
        runtime.stop(StopMode::Drain, None).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pause_interrupts_admission_blocked_on_a_full_queue() -> Result<()> {
        let mut runtime = SourceRuntime::new(descriptor()?, config(1)?, None, None)?;
        let ingress = started(&mut runtime).await?;
        let blocked = {
            let ingress = ingress.clone();
            tokio::spawn(async move { ingress.admit(vec![change("a"), change("b")]).await })
        };
        // Let the batch fill the single queue slot and block on the second.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::timeout(Duration::from_secs(5), runtime.pause(&token(), None))
            .await
            .context("pause must not wait for queue capacity")??;
        let failure = blocked.await?.expect_err("interrupted batch");
        assert_eq!(failure.error.kind, IngressErrorKind::Paused);
        assert_eq!(failure.accepted, 1, "the queued prefix stays accepted");
        runtime.stop(StopMode::Abort, None).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_pause_reopens_admission() -> Result<()> {
        let mut runtime = runtime()?;
        let ingress = started(&mut runtime).await?;
        // Hold an admission guard so Pause cannot settle.
        let guard = ingress.enter_admission().await.expect("guard");
        let cancelled = token();
        cancelled.cancel();
        let error = runtime
            .pause(&cancelled, None)
            .await
            .expect_err("cancelled pause");
        assert_eq!(
            error.kind(),
            drasi_computation_plugin_sdk::lifecycle::LifecycleErrorKind::Cancelled
        );
        assert!(!runtime.is_paused());
        drop(guard);
        runtime.stop(StopMode::Drain, None).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn live_settings_swap_without_restart() -> Result<()> {
        let mut runtime = runtime()?;
        let ingress = started(&mut runtime).await?;
        assert!(ingress.authorize(None, None, true).is_ok());
        let mut secured = config(1024)?;
        secured.security.auth_tokens = vec![crate::config::SecretString::new("0123456789abcdef")];
        runtime.update_live(secured)?;
        assert_eq!(
            ingress.authorize(None, None, true).expect_err("token").kind,
            IngressErrorKind::Unauthorized
        );
        assert!(ingress
            .authorize(None, Some("Bearer 0123456789abcdef"), true)
            .is_ok());
        runtime.stop(StopMode::Drain, None).await?;
        Ok(())
    }
}
