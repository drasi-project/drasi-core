// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bootstrap sessions over the graph's bootstrap edges.
//!
//! A consumer asks each producer connected to its bootstrap input for one
//! snapshot by sending a [`BootstrapRequest`] upstream over the control plane.
//! The producer answers only on the bootstrap pipe to that consumer (see
//! [`BootstrapPorts`]), framing the reply as `begin`, tagged records, and one
//! `complete` or `failed` marker.
//!
//! - [`PipeBootstrapProvider`] is the consumer side. It implements
//!   [`ComputationBootstrapProvider`], so a query or transformer bootstraps
//!   from its bootstrap edges exactly as it would from a local provider.
//! - [`BootstrapProviderComponent`] is a [`ComputationBootstrapper`] graph node
//!   that serves each session from a fresh [`ComputationBootstrapProvider`].
//! - [`BootstrapRelay`] lets a source serve a downstream session from the
//!   provider on its own bootstrap input, replacing the provider's watermarks
//!   with the source's live handover boundary.
//! - [`BootstrapSnapshotServer`] serves a component's committed output
//!   snapshot, such as a query's results, through its bootstrap output.

use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::{FailureSurvival, StorageDurability};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub use drasi_computation_contracts::{
    BootstrapMarker, BootstrapRequest, BootstrapSessionEvent, BootstrapSessionId, BOOTSTRAP_BEGIN,
    BOOTSTRAP_COMPLETE, BOOTSTRAP_FAILED, BOOTSTRAP_RECORD, BOOTSTRAP_REQUEST,
    MAX_BOOTSTRAP_FAILURE_REASON_BYTES, MAX_BOOTSTRAP_SESSION_WATERMARKS,
};

/// Bootstrap output port of a [`BootstrapProviderComponent`].
pub const BOOTSTRAP_PROVIDER_OUTPUT_PORT: &str = "snapshot";

const REQUEST_RETRIES: usize = 200;
const REQUEST_RETRY_DELAY: Duration = Duration::from_millis(10);
const STOP_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn failure_reason(error: impl std::fmt::Display) -> String {
    let mut reason = error.to_string();
    if reason.len() > MAX_BOOTSTRAP_FAILURE_REASON_BYTES {
        let mut end = MAX_BOOTSTRAP_FAILURE_REASON_BYTES;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
    }
    reason
}

fn persistent(state: &dyn BootstrapState) -> bool {
    state.durability().process_restart == FailureSurvival::Guaranteed
}

/// The producer side of one session: replies travel only on the bootstrap
/// edge to the requesting consumer.
pub struct BootstrapReply {
    session: BootstrapSessionId,
    sender: Arc<dyn EnvelopeSender>,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
    finished: bool,
    resume: Option<Option<Bytes>>,
}

/// Sends `state` markers on a reply while the reply is borrowed elsewhere.
#[derive(Clone)]
struct ReplyStateSink {
    session: BootstrapSessionId,
    sender: Arc<dyn EnvelopeSender>,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
}

impl ReplyStateSink {
    async fn send(&self, state: Bytes) -> anyhow::Result<()> {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let envelope = BootstrapMarker::new(self.session, BootstrapSessionEvent::State { state })
            .envelope(
            self.stream.clone(),
            sequence,
            GraphChangeCodec::schema().descriptor().clone(),
        )?;
        self.sender
            .send(envelope)
            .await
            .map(|_| ())
            .map_err(|failure| anyhow::anyhow!("send bootstrap state: {}", failure.error))
    }
}

impl BootstrapReply {
    /// Bind the reply to the edge named by `request`.
    pub fn open(ports: &BootstrapPorts, request: &BootstrapRequest) -> anyhow::Result<Self> {
        let edge = BootstrapOutputEdge {
            output_port: request.output_port.clone(),
            consumer: request.consumer.clone(),
            input_port: request.input_port.clone(),
        };
        let sender = ports.sender(&edge).ok_or_else(|| {
            anyhow::anyhow!(
                "no bootstrap edge from output {} to {}.{}",
                request.output_port,
                request.consumer,
                request.input_port
            )
        })?;
        Ok(Self {
            session: request.session,
            sender,
            stream: StreamId::try_new(format!("bootstrap/{}", request.session))?,
            sequence: Arc::new(AtomicU64::new(0)),
            finished: false,
            resume: request.resume.then(|| request.state.clone()),
        })
    }

    /// For a resumed session, the consumer's saved state to complete with.
    pub fn resumed_state(&self) -> Option<Option<Bytes>> {
        self.resume.clone()
    }

    pub fn session(&self) -> BootstrapSessionId {
        self.session
    }

    /// Whether `complete` or `fail` has been sent.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    async fn send(&self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        self.sender
            .send(envelope)
            .await
            .map(|_| ())
            .map_err(|failure| anyhow::anyhow!("send bootstrap reply: {}", failure.error))
    }

    async fn marker(&mut self, event: BootstrapSessionEvent) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.finished,
            "bootstrap session {} already finished",
            self.session
        );
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let envelope = BootstrapMarker::new(self.session, event).envelope(
            self.stream.clone(),
            sequence,
            GraphChangeCodec::schema().descriptor().clone(),
        )?;
        self.send(envelope).await
    }

    pub async fn begin(&mut self) -> anyhow::Result<()> {
        self.marker(BootstrapSessionEvent::Begin).await
    }

    pub async fn record(&mut self, record: &ChangeEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.finished,
            "bootstrap session {} already finished",
            self.session
        );
        self.send(BootstrapMarker::tag_record(self.session, record)?)
            .await
    }

    fn state_sink(&self) -> ReplyStateSink {
        ReplyStateSink {
            session: self.session,
            sender: self.sender.clone(),
            stream: self.stream.clone(),
            sequence: self.sequence.clone(),
        }
    }

    /// Ask the consumer to retain `state` durably before the session goes on.
    pub async fn state(&mut self, state: Bytes) -> anyhow::Result<()> {
        self.marker(BootstrapSessionEvent::State { state }).await
    }

    pub async fn complete(
        &mut self,
        watermarks: Vec<BootstrapWatermark>,
        state: Option<Bytes>,
    ) -> anyhow::Result<()> {
        self.marker(BootstrapSessionEvent::Complete { watermarks, state })
            .await?;
        self.finished = true;
        Ok(())
    }

    pub async fn fail(&mut self, reason: impl std::fmt::Display) -> anyhow::Result<()> {
        let result = self
            .marker(BootstrapSessionEvent::Failed {
                reason: failure_reason(reason),
            })
            .await;
        self.finished = true;
        result
    }

    /// Finish with `outcome`, reporting an error as the session failure. The
    /// returned error is the session's failure, after it was reported.
    pub async fn finish(
        &mut self,
        outcome: anyhow::Result<(Vec<BootstrapWatermark>, Option<Bytes>)>,
    ) -> anyhow::Result<()> {
        let error = match outcome {
            Ok((watermarks, state)) => match self.complete(watermarks, state).await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            },
            Err(error) => error,
        };
        if !self.finished {
            if let Err(send) = self.fail(format!("{error:#}")).await {
                log::debug!("Could not report bootstrap session failure: {send:#}");
            }
        }
        Err(error)
    }
}

/// One event read from a producer's reply.
#[derive(Debug)]
pub enum BootstrapSessionItem {
    Record(ChangeEnvelope),
    /// Provider state to retain durably; see [`BootstrapSessionEvent::State`].
    State(Bytes),
    Complete {
        watermarks: Vec<BootstrapWatermark>,
        state: Option<Bytes>,
    },
}

/// The consumer side of one session on one bootstrap input edge. Envelopes of
/// other (earlier, abandoned) sessions on the edge are discarded.
pub struct BootstrapSessionReader {
    ports: BootstrapPorts,
    edge: BootstrapInputEdge,
    session: BootstrapSessionId,
    began: bool,
    done: bool,
}

impl BootstrapSessionReader {
    pub fn new(ports: BootstrapPorts, edge: BootstrapInputEdge) -> Self {
        Self {
            ports,
            edge,
            session: BootstrapSessionId::new(),
            began: false,
            done: false,
        }
    }

    pub fn edge(&self) -> &BootstrapInputEdge {
        &self.edge
    }

    pub fn session(&self) -> BootstrapSessionId {
        self.session
    }

    /// Ask the producer for this session's snapshot. `origin` names the
    /// component on whose behalf a relaying component asks.
    pub async fn request(
        &self,
        control: &ComponentControl,
        persistent: bool,
        state: Option<Bytes>,
        origin: Option<ComponentId>,
    ) -> anyhow::Result<()> {
        self.send(control, persistent, state, origin, false).await
    }

    /// Ask the producer to restore the handover its saved `state` describes
    /// for a persistent consumer that already completed its bootstrap. The
    /// session completes without records.
    pub async fn resume(
        &self,
        control: &ComponentControl,
        state: Bytes,
        origin: Option<ComponentId>,
    ) -> anyhow::Result<()> {
        self.send(control, true, Some(state), origin, true).await
    }

    async fn send(
        &self,
        control: &ComponentControl,
        persistent: bool,
        state: Option<Bytes>,
        origin: Option<ComponentId>,
        resume: bool,
    ) -> anyhow::Result<()> {
        let request = BootstrapRequest {
            session: self.session,
            consumer: control.id().clone(),
            input_port: self.edge.input_port.clone(),
            output_port: self.edge.producer.port.clone(),
            persistent,
            state,
            origin: origin.filter(|origin| origin != control.id()),
            resume,
        };
        request.validate()?;
        let mut attempts = 0;
        loop {
            match control.notify_neighbor(
                &self.edge.producer.component,
                ControlNotification::BootstrapRequest(request.clone()),
            ) {
                Ok(()) => return Ok(()),
                Err(ControlError::QueueFull { .. }) if attempts < REQUEST_RETRIES => {
                    attempts += 1;
                    tokio::time::sleep(REQUEST_RETRY_DELAY).await;
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "request bootstrap from {}: {error}",
                        self.edge.producer.component
                    ))
                }
            }
        }
    }

    /// The next record, or the completion. A failed session is an error with
    /// the producer's reason.
    pub async fn next(&mut self) -> anyhow::Result<BootstrapSessionItem> {
        anyhow::ensure!(
            !self.done,
            "bootstrap session {} already finished",
            self.session
        );
        loop {
            let Some(envelope) = self.ports.receive(&self.edge).await? else {
                anyhow::bail!(
                    "bootstrap edge from {} closed before session {} completed",
                    self.edge.producer.component,
                    self.session
                );
            };
            let marker = BootstrapMarker::of(&envelope)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "unframed envelope on the bootstrap edge from {}",
                    self.edge.producer.component
                )
            })?;
            if marker.session != self.session {
                continue;
            }
            match marker.event {
                BootstrapSessionEvent::Begin => {
                    anyhow::ensure!(!self.began, "bootstrap session began twice");
                    self.began = true;
                }
                BootstrapSessionEvent::Record => {
                    anyhow::ensure!(self.began, "bootstrap record before session begin");
                    return Ok(BootstrapSessionItem::Record(BootstrapMarker::untag_record(
                        &envelope,
                    )));
                }
                BootstrapSessionEvent::State { state } => {
                    anyhow::ensure!(self.began, "bootstrap state before session begin");
                    return Ok(BootstrapSessionItem::State(state));
                }
                BootstrapSessionEvent::Complete { watermarks, state } => {
                    anyhow::ensure!(self.began, "bootstrap completion before session begin");
                    self.done = true;
                    return Ok(BootstrapSessionItem::Complete { watermarks, state });
                }
                BootstrapSessionEvent::Failed { reason } => {
                    self.done = true;
                    anyhow::bail!(
                        "bootstrap from {} failed: {reason}",
                        self.edge.producer.component
                    );
                }
            }
        }
    }
}

/// Bootstrap state handed to a provider serving one session: the consumer's
/// durability and the state it saved from an earlier session with this producer.
/// A persistent consumer retains each write durably: the state is sent to it
/// as a `state` marker before the provider continues.
struct SessionState {
    persistent: bool,
    saved: Option<Bytes>,
    written: Mutex<Option<Bytes>>,
    sink: ReplyStateSink,
}

impl SessionState {
    fn new(request: &BootstrapRequest, sink: ReplyStateSink) -> Self {
        Self {
            persistent: request.persistent,
            saved: request.state.clone(),
            written: Mutex::new(None),
            sink,
        }
    }
}

#[async_trait]
impl BootstrapState for SessionState {
    fn durability(&self) -> StorageDurability {
        if self.persistent {
            StorageDurability::LOCAL_PROCESS_RESTART
        } else {
            StorageDurability::VOLATILE
        }
    }

    async fn read(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(lock(&self.written).clone().or_else(|| self.saved.clone()))
    }

    async fn write(&self, state: Bytes) -> anyhow::Result<()> {
        if self.persistent {
            self.sink.send(state.clone()).await?;
        }
        *lock(&self.written) = Some(state);
        Ok(())
    }
}

async fn stream_provider(
    reply: &mut BootstrapReply,
    request: &BootstrapRequest,
    provider: &dyn ComputationBootstrapProvider,
) -> anyhow::Result<(Vec<BootstrapWatermark>, Option<Bytes>)> {
    let mut state = SessionState::new(request, reply.state_sink());
    if request.resume {
        // Restore the handover for a consumer that already has its snapshot.
        let preparation = provider.prepare_with_state(&state).await?;
        anyhow::ensure!(
            preparation == BootstrapPreparation::Ready,
            "bootstrap provider cannot resume this consumer ({preparation:?})"
        );
        anyhow::ensure!(
            !matches!(provider.has_pending_snapshot(), Ok(true)),
            "bootstrap provider requires a new snapshot; the consumer must be reset"
        );
        let completion = match provider.completion_state() {
            Ok(Some(state)) => Some(state),
            _ => lock(&state.written)
                .take()
                .or_else(|| request.state.clone()),
        };
        return Ok((Vec::new(), completion));
    }
    let mut preparation = provider.prepare_with_state(&state).await?;
    if preparation == BootstrapPreparation::RefreshVolatile && !request.persistent {
        // The consumer rebuilds from this snapshot; its earlier state is void.
        state = SessionState {
            saved: None,
            written: Mutex::new(None),
            ..state
        };
        preparation = provider.prepare_with_state(&state).await?;
    }
    anyhow::ensure!(
        preparation == BootstrapPreparation::Ready,
        "bootstrap provider cannot serve this consumer ({preparation:?})"
    );
    let mut snapshot = provider.snapshot_with_state(&state).await?;
    while let Some(record) = snapshot.changes.next().await {
        reply.record(&record?).await?;
    }
    let mut watermarks = std::mem::take(&mut snapshot.watermarks);
    drop(snapshot);
    watermarks.extend(provider.complete_snapshot().await?);
    let completion = if request.persistent {
        match provider.completion_state()? {
            Some(state) => Some(state),
            None => lock(&state.written).take(),
        }
    } else {
        None
    };
    Ok((watermarks, completion))
}

/// Serve one session from `provider` and stop it. A provider error is
/// reported to the consumer as the session failure and returned.
pub async fn serve_bootstrap_session(
    reply: &mut BootstrapReply,
    request: &BootstrapRequest,
    provider: &dyn ComputationBootstrapProvider,
) -> anyhow::Result<()> {
    reply.begin().await?;
    let outcome = stream_provider(reply, request, provider).await;
    let stopped = provider.stop().await;
    reply
        .finish(outcome.and_then(|outcome| stopped.map(|()| outcome)))
        .await
}

async fn ready_snapshot(
    provider: &dyn SnapshotProvider,
    timeout: Duration,
) -> anyhow::Result<OutputSnapshot> {
    tokio::time::timeout(timeout, async {
        loop {
            match provider.snapshot().await {
                Err(OutputReadError::NotReady(_)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                result => return result.map_err(anyhow::Error::from),
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("output snapshot was not ready within {timeout:?}"))?
}

/// Serve one session from a committed output snapshot. The completion carries
/// the output cursor as the handover watermark.
pub async fn serve_output_snapshot(
    reply: &mut BootstrapReply,
    provider: &dyn SnapshotProvider,
    timeout: Duration,
) -> anyhow::Result<()> {
    reply.begin().await?;
    if let Some(state) = reply.resumed_state() {
        return reply.finish(Ok((Vec::new(), state))).await;
    }
    let outcome = async {
        let snapshot = ready_snapshot(provider, timeout).await?;
        anyhow::ensure!(
            snapshot.schema() == GraphChangeCodec::schema().descriptor(),
            "bootstrap requires graph-change output"
        );
        let stream = StreamId::try_new(format!("bootstrap/{}", uuid::Uuid::new_v4()))?;
        for (index, (_, record)) in snapshot.records().clone().into_iter().enumerate() {
            reply
                .record(&super::output_bootstrap::snapshot_record(
                    &stream, index, record,
                )?)
                .await?;
        }
        Ok((
            vec![BootstrapWatermark::from_output(snapshot.cursor())],
            None,
        ))
    }
    .await;
    reply.finish(outcome).await
}

fn accept_request(message: &PeerMessage) -> Option<&BootstrapRequest> {
    let ControlNotification::BootstrapRequest(request) = &message.notification else {
        return None;
    };
    if message.from != request.consumer {
        log::warn!(
            "Ignoring bootstrap request from {} on behalf of {}",
            message.from,
            request.consumer
        );
        return None;
    }
    Some(request)
}

/// Control handler serving a component's committed output through its
/// bootstrap output port. Other control messages are ignored.
pub struct BootstrapSnapshotServer {
    ports: Mutex<BootstrapPorts>,
    snapshot: Arc<dyn SnapshotProvider>,
    timeout: Duration,
}

impl BootstrapSnapshotServer {
    pub fn new(snapshot: Arc<dyn SnapshotProvider>) -> Arc<Self> {
        Self::with_timeout(snapshot, DEFAULT_SNAPSHOT_TIMEOUT)
    }

    /// `timeout` bounds the wait for the output to become ready.
    pub fn with_timeout(snapshot: Arc<dyn SnapshotProvider>, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            ports: Mutex::new(BootstrapPorts::default()),
            snapshot,
            timeout,
        })
    }

    pub fn bind(&self, ports: BootstrapPorts) {
        *lock(&self.ports) = ports;
    }

    pub async fn serve(&self, request: &BootstrapRequest) -> anyhow::Result<()> {
        let ports = lock(&self.ports).clone();
        let mut reply = BootstrapReply::open(&ports, request)?;
        serve_output_snapshot(&mut reply, self.snapshot.as_ref(), self.timeout).await
    }
}

#[async_trait]
impl ControlHandler for BootstrapSnapshotServer {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        anyhow::ensure!(
            !matches!(message.notification, ControlNotification::Custom { .. }),
            "component has no handler for a custom control message"
        );
        if let Some(request) = accept_request(&message) {
            if let Err(error) = self.serve(request).await {
                log::warn!(
                    "Bootstrap session {} for {} failed: {error:#}",
                    request.session,
                    request.consumer
                );
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProducerState {
    component: ComponentId,
    port: PortId,
    state: Bytes,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSessions {
    version: u32,
    producers: Vec<ProducerState>,
}

fn encode_sessions(states: &HashMap<(ComponentId, PortId), Bytes>) -> anyhow::Result<Bytes> {
    let mut producers: Vec<_> = states
        .iter()
        .map(|((component, port), state)| ProducerState {
            component: component.clone(),
            port: port.clone(),
            state: state.clone(),
        })
        .collect();
    producers
        .sort_by(|left, right| (&left.component, &left.port).cmp(&(&right.component, &right.port)));
    Ok(Bytes::from(serde_json::to_vec(&SavedSessions {
        version: 1,
        producers,
    })?))
}

#[derive(Default)]
struct Collected {
    watermarks: Vec<BootstrapWatermark>,
    states: Vec<ProducerState>,
    origins: BTreeMap<StreamId, OutputEndpoint>,
    complete: bool,
}

struct Prepared {
    persistent: bool,
    saved: HashMap<(ComponentId, PortId), Bytes>,
    collected: Arc<Mutex<Collected>>,
}

/// A consumer's bootstrap through its bootstrap edges.
///
/// Each snapshot opens one session per bound bootstrap input and merges them
/// in producer order. Records from a producer that is also one of the
/// consumer's data inputs are attributed to that input; records from a local
/// [`BootstrapProviderComponent`] are not. A persistent consumer saves each
/// producer's completion state and returns it with the next request to that
/// producer. A completed bootstrap is reused by a persistent consumer and
/// rebuilt by a volatile one.
pub struct PipeBootstrapProvider {
    graph_id: Arc<str>,
    component: ComponentId,
    ports: BootstrapPorts,
    control: ComponentControl,
    progress: Option<Arc<QuerySourceProgress>>,
    completed: AtomicBool,
    prepared: Mutex<Option<Prepared>>,
}

impl PipeBootstrapProvider {
    pub fn new(
        graph_id: impl Into<Arc<str>>,
        ports: BootstrapPorts,
        control: ComponentControl,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<Arc<Self>> {
        let graph_id = graph_id.into();
        let component = control.id().clone();
        if let Some(progress) = &progress {
            anyhow::ensure!(
                progress.graph_id() == graph_id.as_ref() && progress.component_id() == &component,
                "bootstrap progress belongs to another component"
            );
        }
        Ok(Arc::new(Self {
            graph_id,
            component,
            ports,
            control,
            progress,
            completed: AtomicBool::new(false),
            prepared: Mutex::new(None),
        }))
    }

    /// Record that the consumer already completed a bootstrap, for a consumer
    /// without a source progress owner.
    pub fn mark_completed(&self) {
        self.completed.store(true, Ordering::Release);
    }

    fn bootstrap_complete(&self) -> bool {
        match &self.progress {
            Some(progress) => progress.snapshot().bootstrap_complete,
            None => self.completed.load(Ordering::Acquire),
        }
    }

    /// A restarted persistent consumer that completed its bootstrap asks each
    /// producer it saved state for to restore that handover, such as a paired
    /// source's live boundary, without a new snapshot.
    async fn resume_producers(
        &self,
        state: &dyn BootstrapState,
        saved: &mut HashMap<(ComponentId, PortId), Bytes>,
    ) -> anyhow::Result<()> {
        let mut changed = false;
        for edge in self.ports.inputs() {
            let mut reader = BootstrapSessionReader::new(self.ports.clone(), edge);
            let key = (
                reader.edge().producer.component.clone(),
                reader.edge().producer.port.clone(),
            );
            let Some(bytes) = saved.get(&key).cloned() else {
                continue;
            };
            reader.resume(&self.control, bytes.clone(), None).await?;
            loop {
                match reader.next().await? {
                    BootstrapSessionItem::Record(_) => {
                        anyhow::bail!("a resumed bootstrap session produced records")
                    }
                    BootstrapSessionItem::State(_) => {}
                    BootstrapSessionItem::Complete { state: next, .. } => {
                        if let Some(next) = next.filter(|next| next != &bytes) {
                            saved.insert(key, next);
                            changed = true;
                        }
                        break;
                    }
                }
            }
        }
        if changed {
            state.write(encode_sessions(saved)?).await?;
        }
        Ok(())
    }

    fn collected(&self) -> anyhow::Result<Arc<Mutex<Collected>>> {
        lock(&self.prepared)
            .as_ref()
            .map(|prepared| prepared.collected.clone())
            .ok_or_else(|| anyhow::anyhow!("graph bootstrap was not prepared"))
    }
}

/// Request a session on `edge`, resuming from the state saved for its producer.
async fn open_reader(
    ports: &BootstrapPorts,
    control: &ComponentControl,
    persistent: bool,
    saved: &HashMap<(ComponentId, PortId), Bytes>,
    edge: BootstrapInputEdge,
) -> anyhow::Result<BootstrapSessionReader> {
    let reader = BootstrapSessionReader::new(ports.clone(), edge);
    let state = saved
        .get(&(
            reader.edge().producer.component.clone(),
            reader.edge().producer.port.clone(),
        ))
        .cloned();
    reader.request(control, persistent, state, None).await?;
    Ok(reader)
}

#[async_trait]
impl ComputationBootstrapProvider for PipeBootstrapProvider {
    fn validate_query_scope(
        &self,
        _: &str,
        graph: &str,
        component: &ComponentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            graph == self.graph_id.as_ref() && component == &self.component,
            "graph bootstrap belongs to a different component"
        );
        Ok(())
    }

    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.progress.clone().map(SourceProgressReader::Local)
    }

    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        anyhow::bail!("graph bootstrap requires the consumer's bootstrap state")
    }

    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        *lock(&self.prepared) = None;
        let persistent = persistent(state);
        if self.bootstrap_complete() && !persistent {
            // Rebuild the whole volatile view, including removals that happened
            // while this consumer was stopped.
            self.completed.store(false, Ordering::Release);
            return Ok(BootstrapPreparation::RefreshVolatile);
        }
        let mut saved = HashMap::new();
        if let Some(bytes) = state.read().await? {
            let sessions: SavedSessions = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                sessions.version == 1,
                "invalid graph bootstrap state version"
            );
            for producer in sessions.producers {
                saved.insert((producer.component, producer.port), producer.state);
            }
        }
        if self.bootstrap_complete() && !saved.is_empty() {
            self.resume_producers(state, &mut saved).await?;
        }
        *lock(&self.prepared) = Some(Prepared {
            persistent,
            saved,
            collected: Arc::default(),
        });
        Ok(BootstrapPreparation::Ready)
    }

    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("graph bootstrap requires the consumer's bootstrap state")
    }

    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let (persistent, saved, collected) = {
            let prepared = lock(&self.prepared);
            let prepared = prepared
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("graph bootstrap was not prepared"))?;
            (
                prepared.persistent,
                prepared.saved.clone(),
                prepared.collected.clone(),
            )
        };
        *lock(&collected) = Collected::default();
        let edges = self.ports.inputs();
        anyhow::ensure!(
            !edges.is_empty(),
            "component {} has no bound bootstrap input",
            self.component
        );
        // Inputs are drained one after another, and each is requested only
        // when the previous one completes. Holding one producer's session
        // while waiting for another's would deadlock consumers that share
        // producers serving one session at a time.
        let mut pending: std::collections::VecDeque<_> = edges.into_iter().collect();
        let first = pending.pop_front().expect("bootstrap inputs are not empty");
        let mut reader = open_reader(&self.ports, &self.control, persistent, &saved, first).await?;
        // Retain the first provider's state before any record is processed, so
        // a restarted session can resume or release what the provider created.
        // A later provider's state is retained when the bootstrap completes.
        let mut retained = saved.clone();
        let first = loop {
            match reader.next().await? {
                BootstrapSessionItem::State(bytes) => {
                    if persistent {
                        let producer = &reader.edge().producer;
                        retained.insert((producer.component.clone(), producer.port.clone()), bytes);
                        state.write(encode_sessions(&retained)?).await?;
                    }
                }
                item => break item,
            }
        };
        let readers: std::collections::VecDeque<_> = [(reader, Some(first))].into();
        let ports = self.ports.clone();
        let control = self.control.clone();
        let changes = futures::stream::try_unfold(
            (readers, pending, collected),
            move |(mut readers, mut pending, collected)| {
                let ports = ports.clone();
                let control = control.clone();
                let saved = saved.clone();
                async move {
                    loop {
                        if readers.is_empty() {
                            let Some(edge) = pending.pop_front() else {
                                lock(&collected).complete = true;
                                return Ok(None);
                            };
                            let reader =
                                open_reader(&ports, &control, persistent, &saved, edge).await?;
                            readers.push_back((reader, None));
                        }
                        let Some((reader, first)) = readers.front_mut() else {
                            unreachable!("a reader was just opened");
                        };
                        let item = match first.take() {
                            Some(item) => item,
                            None => reader.next().await?,
                        };
                        match item {
                            BootstrapSessionItem::State(_) => {
                                log::debug!(
                                "Bootstrap state from {} after its first record is kept only on completion",
                                reader.edge().producer.component
                            );
                            }
                            BootstrapSessionItem::Record(record) => {
                                if !reader.edge().local {
                                    let mut collected = lock(&collected);
                                    let stream = record.system().stream().clone();
                                    let producer = &reader.edge().producer;
                                    match collected.origins.get(&stream) {
                                        Some(origin) => anyhow::ensure!(
                                            origin.component == producer.component,
                                            "bootstrap stream {stream} is shared by two producers"
                                        ),
                                        None => {
                                            collected.origins.insert(stream, producer.clone());
                                        }
                                    }
                                }
                                return Ok(Some((record, (readers, pending, collected))));
                            }
                            BootstrapSessionItem::Complete { watermarks, state } => {
                                let producer = reader.edge().producer.clone();
                                readers.pop_front();
                                let mut collected = lock(&collected);
                                collected.watermarks.extend(watermarks);
                                if let Some(state) = state.filter(|_| persistent) {
                                    collected.states.push(ProducerState {
                                        component: producer.component,
                                        port: producer.port,
                                        state,
                                    });
                                }
                            }
                        }
                    }
                }
            },
        );
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(changes),
            watermarks: Vec::new(),
        })
    }

    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        let collected = self.collected()?;
        let mut collected = lock(&collected);
        anyhow::ensure!(
            collected.complete,
            "graph bootstrap sessions did not complete"
        );
        self.completed.store(true, Ordering::Release);
        Ok(std::mem::take(&mut collected.watermarks))
    }

    fn completion_state(&self) -> anyhow::Result<Option<Bytes>> {
        let prepared = lock(&self.prepared);
        let Some(prepared) = prepared.as_ref() else {
            return Ok(None);
        };
        let collected = lock(&prepared.collected);
        if !prepared.persistent || collected.states.is_empty() {
            return Ok(None);
        }
        let states = collected
            .states
            .iter()
            .map(|state| {
                (
                    (state.component.clone(), state.port.clone()),
                    state.state.clone(),
                )
            })
            .collect();
        Ok(Some(encode_sessions(&states)?))
    }

    fn input_origin(&self, stream: &StreamId) -> anyhow::Result<Option<OutputEndpoint>> {
        let collected = self.collected()?;
        let origin = lock(&collected).origins.get(stream).cloned();
        Ok(origin)
    }

    async fn stop(&self) -> anyhow::Result<()> {
        *lock(&self.prepared) = None;
        Ok(())
    }
}

/// Builds one provider for each bootstrap session.
pub type BootstrapProviderFactory = Arc<
    dyn Fn(&BootstrapRequest) -> anyhow::Result<Arc<dyn ComputationBootstrapProvider>>
        + Send
        + Sync,
>;

struct ActiveSession {
    reply: Arc<tokio::sync::Mutex<BootstrapReply>>,
}

/// A provider node's run state. Requests that arrive while the node is not
/// running wait for it to start; only disposal rejects them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderState {
    Idle,
    Running,
    Disposed,
}

struct ProviderServer {
    ports: Mutex<BootstrapPorts>,
    factory: BootstrapProviderFactory,
    active: Mutex<HashMap<BootstrapSessionId, ActiveSession>>,
    state: tokio::sync::watch::Sender<ProviderState>,
}

impl ProviderServer {
    async fn serve(&self, request: &BootstrapRequest) -> anyhow::Result<()> {
        let ports = lock(&self.ports).clone();
        let reply = Arc::new(tokio::sync::Mutex::new(BootstrapReply::open(
            &ports, request,
        )?));
        let mut state = self.state.subscribe();
        let ready = state
            .wait_for(|state| *state != ProviderState::Idle)
            .await
            .map(|state| *state);
        if !matches!(ready, Ok(ProviderState::Running)) {
            return reply
                .lock()
                .await
                .fail("bootstrap provider is disposed")
                .await;
        }
        lock(&self.active).insert(
            request.session,
            ActiveSession {
                reply: reply.clone(),
            },
        );
        let work = async {
            let mut reply = reply.lock().await;
            match (self.factory)(request) {
                Ok(provider) => {
                    serve_bootstrap_session(&mut reply, request, provider.as_ref()).await
                }
                Err(error) => {
                    reply.begin().await?;
                    reply.finish(Err(error)).await
                }
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = state.wait_for(|state| *state != ProviderState::Running) => None,
            result = work => Some(result),
        };
        let result = match outcome {
            Some(result) => result,
            None => {
                let mut reply = reply.lock().await;
                if reply.is_finished() {
                    Ok(())
                } else {
                    reply.fail("bootstrap provider stopped").await
                }
            }
        };
        lock(&self.active).remove(&request.session);
        result
    }

    async fn cancel(&self, state: ProviderState) {
        self.state.send_replace(state);
        let active: Vec<_> = lock(&self.active)
            .drain()
            .map(|(_, session)| session.reply)
            .collect();
        let cancel = async {
            for reply in active {
                let mut reply = reply.lock().await;
                if !reply.is_finished() {
                    if let Err(error) = reply.fail("bootstrap provider stopped").await {
                        log::debug!("Could not report stopped bootstrap session: {error:#}");
                    }
                }
            }
        };
        if tokio::time::timeout(STOP_GRACE, cancel).await.is_err() {
            log::warn!("Bootstrap provider did not report all stopped sessions in time");
        }
    }
}

#[async_trait]
impl ControlHandler for ProviderServer {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        if let Some(request) = accept_request(&message) {
            if let Err(error) = self.serve(request).await {
                log::warn!(
                    "Bootstrap session {} for {} failed: {error:#}",
                    request.session,
                    request.consumer
                );
            }
        }
        Ok(())
    }
}

/// A graph node that serves bootstrap sessions from a provider factory. Each
/// session gets its own provider, which is stopped when the session ends.
/// Stopping the node fails its in-flight sessions; requests that arrive
/// before the node starts (or while it is stopped) wait until it runs.
pub struct BootstrapProviderComponent {
    descriptor: ComponentDescriptor,
    server: Arc<ProviderServer>,
}

impl BootstrapProviderComponent {
    /// Every provider node's description, shared with factories that build
    /// them: the node keeps no progress of its own and its bootstrap outputs
    /// declare no producer identity, since each serves a requesting consumer's
    /// own session.
    pub fn provider_description() -> super::ComponentDescription {
        super::ComponentDescription::new(super::ProgressPersistence::Stateless)
    }

    /// A node with a single graph-change bootstrap output,
    /// [`BOOTSTRAP_PROVIDER_OUTPUT_PORT`].
    pub fn new(id: ComponentId, factory: BootstrapProviderFactory) -> anyhow::Result<Self> {
        let descriptor = ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::bootstrap(
                PortId::try_new(BOOTSTRAP_PROVIDER_OUTPUT_PORT)?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        Self::with_descriptor(descriptor, factory)
    }

    /// A node with a custom descriptor; it must declare only bootstrap outputs.
    pub fn with_descriptor(
        descriptor: ComponentDescriptor,
        factory: BootstrapProviderFactory,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !descriptor.ports().is_empty()
                && descriptor.ports().iter().all(|port| {
                    port.kind() == PortKind::Bootstrap && port.direction() == PortDirection::Output
                }),
            "a bootstrap provider declares only bootstrap outputs"
        );
        Ok(Self {
            descriptor,
            server: Arc::new(ProviderServer {
                ports: Mutex::new(BootstrapPorts::default()),
                factory,
                active: Mutex::new(HashMap::new()),
                state: tokio::sync::watch::channel(ProviderState::Idle).0,
            }),
        })
    }

    /// Serve every session from one shared provider.
    pub fn shared(
        id: ComponentId,
        provider: Arc<dyn ComputationBootstrapProvider>,
    ) -> anyhow::Result<Self> {
        Self::new(id, Arc::new(move |_| Ok(provider.clone())))
    }
}

#[async_trait]
impl ComputationComponent for BootstrapProviderComponent {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn description(&self) -> super::ComponentDescription {
        Self::provider_description()
    }

    fn bind_bootstrap(&mut self, ports: BootstrapPorts) {
        *lock(&self.server.ports) = ports;
    }

    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        Some(self.server.clone())
    }

    crate::passive_component_lifecycle!(@hook initialize, InitializeContext);
    crate::passive_component_lifecycle!(@hook pause, PauseContext);
    crate::passive_component_lifecycle!(@hook resume, ResumeContext);

    async fn start(&mut self, _: &StartContext) -> anyhow::Result<()> {
        self.server.state.send_replace(ProviderState::Running);
        Ok(())
    }

    async fn stop(&mut self, _: &StopContext) -> anyhow::Result<()> {
        self.server.cancel(ProviderState::Idle).await;
        Ok(())
    }

    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        self.server.cancel(ProviderState::Disposed).await;
        anyhow::ensure!(
            !context.erases_owned_state(),
            "a bootstrap provider owns no state to remove"
        );
        Ok(())
    }
}

impl ComputationBootstrapper for BootstrapProviderComponent {}

/// Maps the upstream provider's completion watermarks to the source's
/// handover boundary: the live stream positions the snapshot covers.
pub type BootstrapHandover = Arc<
    dyn Fn(&BootstrapRequest, &[BootstrapWatermark]) -> anyhow::Result<Vec<BootstrapWatermark>>
        + Send
        + Sync,
>;

/// A source's bootstrap relay. It serves each downstream session from the
/// provider on the source's bootstrap input: one upstream session per
/// downstream session, forwarding records and failures, and completing with
/// the source's handover watermarks and the provider's completion state.
/// Without a bootstrap input the snapshot is empty. Use it as the source's
/// control handler, or call [`Self::relay`] from one.
///
/// A source starting for the very first time, with no saved live position,
/// calls [`Self::hold_live`] and delivers live changes only after
/// [`Self::live_released`]: the first relayed session's boundary, as a legacy
/// source waits for its first bootstrap. Later consumers get their own session
/// and handover boundary while live changes continue, and skip the live
/// changes their snapshot covers.
pub struct BootstrapRelay {
    ports: Mutex<BootstrapPorts>,
    handover: BootstrapHandover,
    released: tokio::sync::watch::Sender<bool>,
    /// Whether the source's bootstrap input is bound for serving requests.
    active: tokio::sync::watch::Sender<bool>,
    /// Whether a relayed session completed during this activation.
    boundary_reached: Mutex<bool>,
}

impl BootstrapRelay {
    pub fn new(handover: BootstrapHandover) -> Arc<Self> {
        Arc::new(Self {
            ports: Mutex::new(BootstrapPorts::default()),
            handover,
            released: tokio::sync::watch::channel(true).0,
            active: tokio::sync::watch::channel(true).0,
            boundary_reached: Mutex::new(false),
        })
    }

    /// Start a new activation: nothing is held and no boundary is reached.
    /// Requests deferred by [`Self::end_activation`] are served from here on.
    pub fn begin_activation(&self) {
        *lock(&self.boundary_reached) = false;
        self.released.send_replace(true);
        self.active.send_replace(true);
    }

    /// Defer bootstrap requests until the next [`Self::begin_activation`].
    ///
    /// A consumer can start before its source does. Its request must not be
    /// answered before the source's activation binds the provider input, or
    /// the consumer would complete an empty bootstrap.
    pub fn end_activation(&self) {
        self.active.send_replace(false);
    }

    pub fn bind(&self, ports: BootstrapPorts) {
        *lock(&self.ports) = ports;
    }

    /// Whether a provider is connected to the source's bootstrap input.
    pub fn has_provider(&self) -> bool {
        !lock(&self.ports).inputs().is_empty()
    }

    /// Whether a provider is connected and at least one consumer bootstraps
    /// from the source's bootstrap output, so a first boundary will come.
    pub fn relays_sessions(&self) -> bool {
        let ports = lock(&self.ports);
        !ports.inputs().is_empty() && !ports.outputs().is_empty()
    }

    /// Hold live delivery until the first relayed session of this activation
    /// completes. Without a connected provider and bootstrap consumer there is
    /// no boundary to wait for, and once a session has completed there is
    /// nothing left to hold.
    pub fn hold_live(&self) {
        let reached = lock(&self.boundary_reached);
        if self.relays_sessions() && !*reached {
            self.released.send_replace(false);
        }
    }

    /// Release a hold without a completed session, for example when the
    /// source stops.
    pub fn release_live(&self) {
        self.released.send_replace(true);
    }

    /// Whether live delivery is currently held.
    pub fn is_live_held(&self) -> bool {
        !*self.released.borrow()
    }

    /// Wait until live delivery is not held.
    pub async fn live_released(&self) {
        let mut released = self.released.subscribe();
        // The sender lives as long as `self`, so the wait cannot fail.
        let _ = released.wait_for(|released| *released).await;
    }

    pub async fn relay(
        &self,
        control: &ComponentControl,
        request: &BootstrapRequest,
    ) -> anyhow::Result<()> {
        let mut active = self.active.subscribe();
        // The sender lives as long as `self`, so the wait cannot fail.
        let _ = active.wait_for(|active| *active).await;
        let ports = lock(&self.ports).clone();
        let mut reply = BootstrapReply::open(&ports, request)?;
        reply.begin().await?;
        let outcome = async {
            let inputs = ports.inputs();
            anyhow::ensure!(
                inputs.len() <= 1,
                "a source has at most one bootstrap provider"
            );
            let Some(edge) = inputs.into_iter().next() else {
                if request.resume {
                    return Ok((Vec::new(), request.state.clone()));
                }
                return Ok(((self.handover)(request, &[])?, None));
            };
            let mut upstream = BootstrapSessionReader::new(ports.clone(), edge);
            match (&request.state, request.resume) {
                (Some(state), true) => {
                    upstream
                        .resume(control, state.clone(), Some(request.origin().clone()))
                        .await?
                }
                _ => {
                    upstream
                        .request(
                            control,
                            request.persistent,
                            request.state.clone(),
                            Some(request.origin().clone()),
                        )
                        .await?
                }
            }
            loop {
                match upstream.next().await? {
                    BootstrapSessionItem::Record(record) => {
                        anyhow::ensure!(
                            !request.resume,
                            "a resumed bootstrap session produced records"
                        );
                        reply.record(&record).await?
                    }
                    BootstrapSessionItem::State(state) => reply.state(state).await?,
                    // A resumed consumer keeps its live position; there is no
                    // new handover boundary.
                    BootstrapSessionItem::Complete { state, .. } if request.resume => {
                        return Ok((Vec::new(), state))
                    }
                    BootstrapSessionItem::Complete { watermarks, state } => {
                        return Ok(((self.handover)(request, &watermarks)?, state))
                    }
                }
            }
        }
        .await;
        let completed = outcome.is_ok();
        let result = reply.finish(outcome).await;
        if completed && result.is_ok() {
            *lock(&self.boundary_reached) = true;
            self.release_live();
        }
        result
    }
}

#[async_trait]
impl ControlHandler for BootstrapRelay {
    async fn on_message(
        &self,
        message: PeerMessage,
        control: ComponentControl,
    ) -> anyhow::Result<()> {
        if let Some(request) = accept_request(&message) {
            if let Err(error) = self.relay(&control, request).await {
                log::warn!(
                    "Relayed bootstrap session {} for {} failed: {error:#}",
                    request.session,
                    request.consumer
                );
            }
        }
        Ok(())
    }
}
