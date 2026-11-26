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
//!   snapshot or retained suffix through its bootstrap output. A persistent
//!   consumer saves the producer at the snapshot cut and supplies its own
//!   applied logical position on restart; replay completion advances no cursor.

use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::{FailureSurvival, StorageDurability};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub use drasi_computation_contracts::{
    BootstrapLabels, BootstrapMarker, BootstrapReplayError, BootstrapRequest,
    BootstrapSessionEvent, BootstrapSessionId, BOOTSTRAP_BEGIN, BOOTSTRAP_COMPLETE,
    BOOTSTRAP_FAILED, BOOTSTRAP_RECORD, BOOTSTRAP_REQUEST, MAX_BOOTSTRAP_FAILURE_REASON_BYTES,
    MAX_BOOTSTRAP_SESSION_WATERMARKS,
};

/// Bootstrap output port of a [`BootstrapProviderComponent`].
pub const BOOTSTRAP_PROVIDER_OUTPUT_PORT: &str = "snapshot";

/// The derivation name of a query's owned scheduled-notification helper
/// (`ComponentId::derive_owned(query, QUERY_SCHEDULED_HELPER)`).
pub const QUERY_SCHEDULED_HELPER: &str = "query-scheduled";

/// The source whose element namespace a provider connected to `consumer`'s
/// bootstrap input serves, and the stream its records belong to.
///
/// A consumer with no data input is a source relaying its consumers'
/// sessions: the provider serves the source itself and its single output
/// stream. Any other consumer (a query or transformer with a local provider)
/// is served the namespace of the single producer of its data inputs (its own
/// scheduled-notification helper aside). `None`
/// when that is ambiguous (no single source or stream). The graph applies
/// the same rule when it binds the provider's edge, so a provider never
/// names the source it serves.
pub fn served_source<'a>(
    consumer: &ComponentId,
    node: impl Fn(&ComponentId) -> Option<&'a NodeSnapshot>,
    edges: impl IntoIterator<Item = &'a EdgeDefinition>,
) -> Option<(ComponentId, StreamId)> {
    let consumer_node = node(consumer)?;
    let data_inputs: std::collections::BTreeSet<&PortId> = consumer_node
        .descriptor
        .ports()
        .iter()
        .filter(|port| port.direction() == PortDirection::Input && port.kind() == PortKind::Data)
        .map(|port| port.id())
        .collect();
    if data_inputs.is_empty() {
        let streams: std::collections::BTreeSet<&StreamId> =
            consumer_node.output_streams.values().collect();
        let mut streams = streams.into_iter();
        return match (streams.next(), streams.next()) {
            (Some(stream), None) => Some((consumer.clone(), stream.clone())),
            _ => None,
        };
    }
    // The consumer's own scheduled-notification helper produces no source
    // elements, so it has no namespace to serve.
    let scheduled = ComponentId::derive_owned(consumer, QUERY_SCHEDULED_HELPER).ok();
    let producers: std::collections::BTreeSet<&Endpoint> = edges
        .into_iter()
        .filter(|edge| &edge.to.component == consumer && data_inputs.contains(&edge.to.port))
        .map(|edge| &edge.from)
        .filter(|endpoint| scheduled.as_ref() != Some(&endpoint.component))
        .collect();
    let mut producers = producers.into_iter();
    let (Some(producer), None) = (producers.next(), producers.next()) else {
        return None;
    };
    let stream = node(&producer.component)?
        .output_streams
        .get(&producer.port)?
        .clone();
    Some((producer.component.clone(), stream))
}

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
    lifetime: super::bootstrap_ports::ReadLifetime,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
    finished: bool,
}

/// Sends `state` markers on a reply while the reply is borrowed elsewhere.
#[derive(Clone)]
struct ReplyStateSink {
    session: BootstrapSessionId,
    sender: Arc<dyn EnvelopeSender>,
    lifetime: super::bootstrap_ports::ReadLifetime,
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
        tokio::select! {
            biased;
            _ = self.lifetime.clone().ended() => anyhow::bail!("bootstrap reader retired"),
            sent = self.sender.send(envelope) => sent.map_err(|failure| anyhow::anyhow!("send bootstrap state: {}", failure.error))?,
        };
        self.lifetime.clone().intent_committed(sequence).await
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
        let (sender, lifetime) = ports.reply_to(&edge, request.session).ok_or_else(|| {
            anyhow::anyhow!(
                "no active bootstrap session from output {} to {}.{}",
                request.output_port,
                request.consumer,
                request.input_port
            )
        })?;
        Self::with_sender(request.session, sender, lifetime)
    }

    fn with_sender(
        session: BootstrapSessionId,
        sender: Arc<dyn EnvelopeSender>,
        lifetime: super::bootstrap_ports::ReadLifetime,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            session,
            sender,
            lifetime,
            stream: StreamId::try_new(format!("bootstrap/{session}"))?,
            sequence: Arc::new(AtomicU64::new(0)),
            finished: false,
        })
    }

    pub fn session(&self) -> BootstrapSessionId {
        self.session
    }

    /// Whether `complete` or `fail` has been sent.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    async fn send(&self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        tokio::select! {
            biased;
            _ = self.lifetime.clone().ended() => anyhow::bail!("bootstrap reader retired"),
            sent = self.sender.send(envelope) => sent
                .map(|_| ())
                .map_err(|failure| anyhow::anyhow!("send bootstrap reply: {}", failure.error)),
        }
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
            lifetime: self.lifetime.clone(),
            stream: self.stream.clone(),
            sequence: self.sequence.clone(),
        }
    }

    /// Ask the consumer to retain `state` durably before the session goes on.
    pub async fn state(&mut self, state: Bytes) -> anyhow::Result<()> {
        anyhow::ensure!(!self.finished, "bootstrap session already finished");
        self.state_sink().send(state).await
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
    Replay(ChangeEnvelope),
    OutputComplete(OutputCursor),
    /// Provider state to retain durably; see [`BootstrapSessionEvent::State`].
    State(BootstrapStateIntent),
    Complete {
        watermarks: Vec<BootstrapWatermark>,
        state: Option<Bytes>,
    },
}

/// An opaque provider intent, acknowledged only after it has been durably
/// written by the consumer (or that write has been confirmed through a relay).
pub struct BootstrapStateIntent {
    state: Bytes,
    session: BootstrapSessionId,
    sequence: u64,
    binding: Option<super::bootstrap_ports::SharedReceiver>,
}

impl std::fmt::Debug for BootstrapStateIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapStateIntent")
            .field("session", &self.session)
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}

impl BootstrapStateIntent {
    pub fn state(&self) -> &Bytes {
        &self.state
    }

    /// Confirm the consumer's successful checkpoint write, never receipt of the marker.
    pub fn acknowledge(mut self) -> anyhow::Result<()> {
        self.binding
            .take()
            .expect("unconsumed intent")
            .confirm_intent(self.session, self.sequence)
    }
}

impl Drop for BootstrapStateIntent {
    fn drop(&mut self) {
        if let Some(binding) = &self.binding {
            binding.release(self.session);
        }
    }
}

/// The consumer side of one session on one bootstrap input edge. Envelopes of
/// other (earlier, abandoned) sessions on the edge are discarded.
pub struct BootstrapSessionReader {
    ports: BootstrapPorts,
    binding: Option<super::bootstrap_ports::SharedReceiver>,
    edge: BootstrapInputEdge,
    session: BootstrapSessionId,
    labels: Option<BootstrapLabels>,
    began: bool,
    done: bool,
}

impl BootstrapSessionReader {
    pub fn new(ports: BootstrapPorts, edge: BootstrapInputEdge) -> Self {
        let session = BootstrapSessionId::new();
        Self {
            binding: ports.begin_read(&edge, session),
            ports,
            edge,
            session,
            labels: None,
            began: false,
            done: false,
        }
    }

    /// Ask for only the elements carrying these labels (the origin's declared
    /// labels); `None` asks for everything the producer is configured for.
    pub fn with_labels(mut self, labels: Option<BootstrapLabels>) -> Self {
        self.labels = labels;
        self
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
        self.send(control, persistent, state, origin, false, None)
            .await
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
        self.send(control, true, Some(state), origin, true, None)
            .await
    }

    async fn replay_after(
        &mut self,
        control: &ComponentControl,
        after: &OutputCursor,
        replay: &mut Vec<ChangeEnvelope>,
    ) -> anyhow::Result<()> {
        self.send(control, true, None, None, false, Some(after.clone()))
            .await?;
        let mut sequence = after.sequence();
        loop {
            match self.next().await? {
                BootstrapSessionItem::Replay(record) => {
                    let position = GraphProducerProgress::from_envelope(&record)?
                        .ok_or_else(|| anyhow::anyhow!("output replay lacks producer progress"))?;
                    anyhow::ensure!(
                        position.identity() == after.producer()
                            && sequence.checked_add(1) == Some(position.sequence())
                            && record.changes().schema() == GraphChangeCodec::schema().descriptor(),
                        "invalid output replay after sequence {sequence}"
                    );
                    sequence = position.sequence();
                    replay.push(record);
                }
                BootstrapSessionItem::OutputComplete(head) => {
                    anyhow::ensure!(
                        head.producer() == after.producer() && sequence == head.sequence(),
                        "output replay did not reach its committed head"
                    );
                    return Ok(());
                }
                _ => anyhow::bail!("expected retained output, not a bootstrap snapshot"),
            }
        }
    }

    async fn send(
        &self,
        control: &ComponentControl,
        persistent: bool,
        state: Option<Bytes>,
        origin: Option<ComponentId>,
        resume: bool,
        replay_after: Option<OutputCursor>,
    ) -> anyhow::Result<()> {
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bootstrap input is not bound"))?;
        let request = BootstrapRequest {
            session: self.session,
            consumer: control.id().clone(),
            input_port: self.edge.input_port.clone(),
            output_port: self.edge.producer.port.clone(),
            persistent,
            state,
            origin: origin.filter(|origin| origin != control.id()),
            resume,
            replay_after: replay_after.map(Box::new),
            labels: self.labels.clone(),
        };
        request.validate()?;
        let mut attempts = 0;
        loop {
            anyhow::ensure!(
                self.ports.is_current(&self.edge, binding),
                "bootstrap binding was replaced"
            );
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
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("bootstrap input is not bound"))?;
        loop {
            let Some(envelope) = self
                .ports
                .receive_on(&self.edge, binding, Some(self.session))
                .await?
            else {
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
                BootstrapSessionEvent::Replay { control } => {
                    anyhow::ensure!(self.began, "bootstrap replay before session begin");
                    return Ok(BootstrapSessionItem::Replay(BootstrapMarker::untag_replay(
                        &envelope, control,
                    )));
                }
                BootstrapSessionEvent::OutputComplete(cursor) => {
                    anyhow::ensure!(self.began, "output completion before session begin");
                    self.done = true;
                    return Ok(BootstrapSessionItem::OutputComplete(cursor));
                }
                BootstrapSessionEvent::ReplayFailed(error) => {
                    self.done = true;
                    return Err(OutputReadError::from(error).into());
                }
                BootstrapSessionEvent::State { state } => {
                    anyhow::ensure!(self.began, "bootstrap state before session begin");
                    return Ok(BootstrapSessionItem::State(BootstrapStateIntent {
                        state,
                        session: self.session,
                        sequence: envelope.system().sequence(),
                        binding: Some(binding.clone()),
                    }));
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

impl Drop for BootstrapSessionReader {
    fn drop(&mut self) {
        if let Some(binding) = &self.binding {
            binding.release(self.session);
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
    written: tokio::sync::Mutex<Option<Bytes>>,
    sink: ReplyStateSink,
}

impl SessionState {
    fn new(request: &BootstrapRequest, sink: ReplyStateSink) -> Self {
        Self {
            persistent: request.persistent,
            saved: request.state.clone(),
            written: Default::default(),
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
        Ok(self
            .written
            .lock()
            .await
            .clone()
            .or_else(|| self.saved.clone()))
    }

    async fn write(&self, state: Bytes) -> anyhow::Result<()> {
        let mut written = self.written.lock().await;
        if self.persistent {
            self.sink.send(state.clone()).await?;
        }
        *written = Some(state);
        Ok(())
    }
}

async fn stream_provider(
    reply: &mut BootstrapReply,
    request: &BootstrapRequest,
    provider: &dyn ComputationBootstrapProvider,
) -> anyhow::Result<(Vec<BootstrapWatermark>, Option<Bytes>)> {
    anyhow::ensure!(
        request.replay_after.is_none(),
        "a bootstrap provider cannot replay query output"
    );
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
            _ => state.read().await?,
        };
        return Ok((Vec::new(), completion));
    }
    let mut preparation = provider.prepare_with_state(&state).await?;
    if preparation == BootstrapPreparation::RefreshVolatile && !request.persistent {
        // The consumer rebuilds from this snapshot; its earlier state is void.
        state = SessionState {
            saved: None,
            written: Default::default(),
            ..state
        };
        preparation = provider.prepare_with_state(&state).await?;
    }
    anyhow::ensure!(
        preparation == BootstrapPreparation::Ready,
        "bootstrap provider cannot serve this consumer ({preparation:?})"
    );
    let state = Arc::new(state);
    let mut snapshot = provider.open_snapshot(state.clone()).await?;
    while let Some(record) = snapshot.changes.next().await {
        reply.record(&record?).await?;
    }
    let mut watermarks = std::mem::take(&mut snapshot.watermarks);
    drop(snapshot);
    watermarks.extend(provider.complete_snapshot().await?);
    let completion = if request.persistent {
        match provider.completion_state()? {
            Some(state) => Some(state),
            None => state.written.lock().await.take(),
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
    run_provider(reply, request, provider, std::future::pending()).await?
}

// A failed read belongs to the requester. A failed stop also leaves the
// provider owned by the node until cleanup can be retried.
async fn run_provider(
    reply: &mut BootstrapReply,
    request: &BootstrapRequest,
    provider: &dyn ComputationBootstrapProvider,
    stopping: impl std::future::Future<Output = ()>,
) -> anyhow::Result<anyhow::Result<()>> {
    let lifetime = reply.lifetime.clone();
    let outcome = tokio::select! {
        biased;
        _ = lifetime.ended() => None,
        _ = stopping => Some(Err(anyhow::anyhow!("bootstrap provider stopped"))),
        result = async {
            reply.begin().await?;
            stream_provider(reply, request, provider).await
        } => Some(result),
    };
    if let Err(error) = provider.stop().await {
        if outcome.is_some() {
            if let Err(report) = reply
                .fail(format!("bootstrap provider cleanup failed: {error:#}"))
                .await
            {
                log::debug!("Could not report bootstrap cleanup failure: {report:#}");
            }
        }
        return Err(error.context("bootstrap provider cleanup failed"));
    }
    Ok(match outcome {
        Some(outcome) => reply.finish(outcome).await,
        None => Ok(()),
    })
}

async fn read_when_ready<
    T,
    F: std::future::Future<Output = std::result::Result<T, OutputReadError>>,
>(
    mut read: impl FnMut() -> F,
    timeout: Duration,
) -> anyhow::Result<T> {
    tokio::time::timeout(timeout, async {
        loop {
            match read().await {
                Err(OutputReadError::NotReady(_)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                result => return result.map_err(anyhow::Error::from),
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("output was not ready within {timeout:?}"))?
}

/// An initial read serves rows and their cut. A persistent restart reads
/// only the retained suffix; its head is not a new bootstrap watermark.
pub async fn serve_output_snapshot(
    reply: &mut BootstrapReply,
    request: &BootstrapRequest,
    provider: &dyn SnapshotProvider,
    replay: Option<&dyn ReplayProvider>,
    timeout: Duration,
) -> anyhow::Result<()> {
    reply.begin().await?;
    let outcome = async {
        anyhow::ensure!(
            !request.resume,
            "query output requires an applied replay cursor, not provider state"
        );
        let head = if let Some(after) = request.replay_after.as_deref() {
            let replay =
                replay.ok_or_else(|| anyhow::anyhow!("output does not provide retained replay"))?;
            let suffix = read_when_ready(|| replay.replay_after(after), timeout).await?;
            reply.retained(after, suffix).await?
        } else {
            let snapshot = read_when_ready(|| provider.snapshot(), timeout).await?;
            anyhow::ensure!(
                snapshot.schema() == GraphChangeCodec::schema().descriptor(),
                "bootstrap requires graph-change output"
            );
            anyhow::ensure!(
                !request.persistent
                    || (snapshot.cursor().producer().persistent() && replay.is_some()),
                "persistent bootstrap requires a durable, replayable output"
            );
            let stream = StreamId::try_new(format!("bootstrap/{}", uuid::Uuid::new_v4()))?;
            for batch in SnapshotBatches::new(&snapshot, stream, SnapshotBatchLimits::default()) {
                reply.record(&batch?).await?;
            }
            snapshot.cursor().clone()
        };
        Ok(head)
    }
    .await;
    reply.finish_output(request, outcome).await
}

impl BootstrapReply {
    async fn retained(
        &self,
        after: &OutputCursor,
        suffix: OutputReplay,
    ) -> anyhow::Result<OutputCursor> {
        anyhow::ensure!(
            suffix.after() == after,
            "output replay answered a different cursor"
        );
        for retained in suffix.envelopes() {
            self.send(BootstrapMarker::tag_replay(self.session, retained)?)
                .await?;
        }
        Ok(suffix.head().clone())
    }

    async fn finish_output(
        &mut self,
        request: &BootstrapRequest,
        read: anyhow::Result<OutputCursor>,
    ) -> anyhow::Result<()> {
        let outcome = match read {
            Ok(head) => {
                self.marker(BootstrapSessionEvent::OutputComplete(head))
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = &outcome {
            let failure = request
                .replay_after
                .as_deref()
                .and_then(|after| {
                    BootstrapReplayError::from_read(error.downcast_ref::<OutputReadError>()?, after)
                })
                .map(BootstrapSessionEvent::ReplayFailed)
                .unwrap_or_else(|| BootstrapSessionEvent::Failed {
                    reason: failure_reason(format!("{error:#}")),
                });
            if let Err(send) = self.marker(failure).await {
                log::debug!("Could not report output read failure: {send:#}");
            }
        }
        self.finished = true;
        outcome
    }
}

/// A registered materialized reader is addressed by its actual data port.
/// Capture the binding's sender once; this job never takes the live instance.
pub(super) fn materialized_session(
    ports: &BootstrapPorts,
    catalogue: &OutputCatalogue,
    producer: &ComponentId,
    message: &PeerMessage,
) -> Option<futures::future::BoxFuture<'static, ()>> {
    use futures::FutureExt;
    let request = accept_request(message)?;
    if message.direction != ControlDirection::Upstream {
        return None;
    }
    let edge = BootstrapOutputEdge {
        output_port: request.output_port.clone(),
        consumer: request.consumer.clone(),
        input_port: request.input_port.clone(),
    };
    let delivery = ports.materialized_output(&edge, request.session)?;
    let super::bootstrap_ports::StateDelivery::Open {
        sender,
        options,
        lifetime,
    } = delivery
    else {
        return Some(futures::future::ready(()).boxed());
    };
    let endpoint = OutputEndpoint::new(producer.clone(), request.output_port.clone());
    let catalogue = catalogue.clone();
    let request = request.clone();
    Some(
        async move {
            let abandoned = lifetime.clone().ended();
            let serve = async {
                let mut reply = BootstrapReply::with_sender(request.session, sender, lifetime)?;
                reply.begin().await?;
                let read = async {
                    request.validate()?;
                    anyhow::ensure!(
                        !request.resume,
                        "materialized output needs an applied replay cursor"
                    );
                    if let Some(after) = request.replay_after.as_deref() {
                        let suffix = catalogue
                            .read_when_ready(&endpoint, |handle| async move {
                                handle.replay_after(after).await
                            })
                            .await?;
                        return reply.retained(after, suffix).await;
                    }
                    let persistent = request.persistent;
                    let mut state = catalogue
                        .read_when_ready(&endpoint, |handle| async move {
                            if persistent && !handle.provides_replay() {
                                return Err(OutputReadError::invalid(
                                    "persistent bootstrap requires retained replay",
                                ));
                            }
                            handle.current_state(options).await
                        })
                        .await?;
                    let cut = state
                        .cursor()
                        .ok_or_else(|| {
                            anyhow::anyhow!("materialized bootstrap requires a logical output cut")
                        })?
                        .clone();
                    anyhow::ensure!(
                        !persistent || cut.producer().persistent(),
                        "persistent bootstrap from a volatile output"
                    );
                    while let Some(batch) = state.next_batch().await? {
                        reply.record(&batch).await?;
                    }
                    Ok(cut)
                }
                .await;
                reply.finish_output(&request, read).await
            };
            let result = tokio::select! {
                biased;
                _ = abandoned => return,
                result = serve => result,
            };
            if let Err(error) = result {
                log::warn!(
                    "State session {} on {endpoint} failed: {error:#}",
                    request.session
                );
            }
        }
        .boxed(),
    )
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
    replay: Option<Arc<dyn ReplayProvider>>,
    timeout: Duration,
}

impl BootstrapSnapshotServer {
    pub fn new(
        snapshot: Arc<dyn SnapshotProvider>,
        replay: Option<Arc<dyn ReplayProvider>>,
    ) -> Arc<Self> {
        Self::with_timeout(snapshot, replay, DEFAULT_SNAPSHOT_TIMEOUT)
    }

    /// `timeout` bounds the wait for the output to become ready.
    pub fn with_timeout(
        snapshot: Arc<dyn SnapshotProvider>,
        replay: Option<Arc<dyn ReplayProvider>>,
        timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            ports: Mutex::new(BootstrapPorts::default()),
            snapshot,
            replay,
            timeout,
        })
    }

    pub fn bind(&self, ports: BootstrapPorts) {
        *lock(&self.ports) = ports;
    }

    pub async fn serve(&self, request: &BootstrapRequest) -> anyhow::Result<()> {
        let ports = lock(&self.ports).clone();
        let mut reply = BootstrapReply::open(&ports, request)?;
        serve_output_snapshot(
            &mut reply,
            request,
            self.snapshot.as_ref(),
            self.replay.as_deref(),
            self.timeout,
        )
        .await
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

/// Opaque provider state is returned verbatim (including during an interrupted
/// first start). Output readers instead need the identity captured at the cut
/// and the *current applied* checkpoint, not an old snapshot sequence.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Handover {
    Output { producer: GraphProducerIdentity },
    Provider(Option<Bytes>),
}

type Handovers = BTreeMap<OutputEndpoint, Handover>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProducerState {
    component: ComponentId,
    port: PortId,
    state: Handover,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSessions {
    version: u32,
    producers: Vec<ProducerState>,
}

fn encode_sessions(states: &Handovers) -> anyhow::Result<Bytes> {
    let producers = states
        .iter()
        .map(|(endpoint, state)| ProducerState {
            component: endpoint.component.clone(),
            port: endpoint.port.clone(),
            state: state.clone(),
        })
        .collect();
    Ok(Bytes::from(serde_json::to_vec(&SavedSessions {
        version: 1,
        producers,
    })?))
}

async fn saved_handovers(state: &dyn BootstrapState) -> anyhow::Result<Handovers> {
    let mut saved = Handovers::new();
    if let Some(bytes) = state.read().await? {
        let sessions: SavedSessions = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            sessions.version == 1,
            "invalid graph bootstrap state version"
        );
        for producer in sessions.producers {
            anyhow::ensure!(
                saved
                    .insert(
                        OutputEndpoint::new(producer.component, producer.port),
                        producer.state
                    )
                    .is_none(),
                "duplicate saved bootstrap producer"
            );
        }
    }
    Ok(saved)
}

#[derive(Default)]
struct Collected {
    watermarks: Vec<BootstrapWatermark>,
    states: Handovers,
    origins: BTreeMap<StreamId, OutputEndpoint>,
    complete: bool,
}

struct Prepared {
    persistent: bool,
    saved: Handovers,
    replay: Vec<ChangeEnvelope>,
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
    labels: Option<BootstrapLabels>,
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
        Self::new_with_labels(graph_id, ports, control, progress, None)
    }

    /// A consumer that declares the labels it reads (a query) asks every
    /// producer for only those labels; relays forward them to their providers.
    pub fn new_with_labels(
        graph_id: impl Into<Arc<str>>,
        ports: BootstrapPorts,
        control: ComponentControl,
        progress: Option<Arc<QuerySourceProgress>>,
        labels: Option<BootstrapLabels>,
    ) -> anyhow::Result<Arc<Self>> {
        if let Some(labels) = &labels {
            labels.validate()?;
        }
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
            labels,
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
    /// producer to restore its own kind of handover without a new snapshot.
    async fn resume_producers(
        &self,
        state: &dyn BootstrapState,
        saved: &mut Handovers,
        replay: &mut Vec<ChangeEnvelope>,
    ) -> anyhow::Result<BootstrapPreparation> {
        let edges = self.ports.inputs();
        if saved.len() != edges.len()
            || edges.iter().any(|edge| !saved.contains_key(&edge.producer))
        {
            // A completed consumer cannot use some other binding's checkpoint,
            // including an older snapshot that saved no output owner.
            return Ok(BootstrapPreparation::ResetRequired);
        }
        let mut changed = false;
        for edge in edges {
            let endpoint = edge.producer.clone();
            let mut reader = BootstrapSessionReader::new(self.ports.clone(), edge);
            let bytes = match &saved[&endpoint] {
                Handover::Output { producer } => {
                    let progress = self
                        .progress
                        .as_ref()
                        .ok_or_else(|| {
                            anyhow::anyhow!("output replay requires committed input progress")
                        })?
                        .snapshot();
                    let Some(checkpoint) = progress
                        .checkpoints
                        .get(&SourceProgressKey::Stream(producer.stream().clone()))
                    else {
                        return Ok(BootstrapPreparation::ResetRequired);
                    };
                    anyhow::ensure!(
                        producer.component_id() == &endpoint.component
                            && producer.graph_id() == self.graph_id.as_ref(),
                        "saved output owner belongs to a different bootstrap edge"
                    );
                    let after = OutputCursor::try_new(producer.clone(), checkpoint.sequence)?;
                    reader.replay_after(&self.control, &after, replay).await?;
                    continue;
                }
                Handover::Provider(None) => continue,
                Handover::Provider(Some(bytes)) => bytes.clone(),
            };
            reader.resume(&self.control, bytes.clone(), None).await?;
            loop {
                match reader.next().await? {
                    BootstrapSessionItem::State(intent) => {
                        retain_intent(state, saved, &endpoint, intent).await?;
                    }
                    BootstrapSessionItem::Complete { state: next, .. } => {
                        if let Some(next) = next.filter(|next| !matches!(saved.get(&endpoint), Some(Handover::Provider(Some(old))) if old == next)) {
                            saved.insert(endpoint, Handover::Provider(Some(next)));
                            changed = true;
                        }
                        break;
                    }
                    _ => anyhow::bail!("unexpected data in provider handover resume"),
                }
            }
        }
        if changed {
            state.write(encode_sessions(saved)?).await?;
        }
        Ok(BootstrapPreparation::Ready)
    }

    fn collected(&self) -> anyhow::Result<Arc<Mutex<Collected>>> {
        lock(&self.prepared)
            .as_ref()
            .map(|prepared| prepared.collected.clone())
            .ok_or_else(|| anyhow::anyhow!("graph bootstrap was not prepared"))
    }
}

async fn retain_intent(
    state: &dyn BootstrapState,
    saved: &mut Handovers,
    endpoint: &OutputEndpoint,
    intent: BootstrapStateIntent,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        persistent(state),
        "a volatile reader cannot confirm a durable bootstrap intent"
    );
    saved.insert(
        endpoint.clone(),
        Handover::Provider(Some(intent.state().clone())),
    );
    state.write(encode_sessions(saved)?).await?;
    intent.acknowledge()
}

/// Request a session on `edge`, resuming from the state saved for its producer.
async fn open_reader(
    ports: &BootstrapPorts,
    control: &ComponentControl,
    persistent: bool,
    saved: &Handovers,
    edge: BootstrapInputEdge,
    labels: Option<BootstrapLabels>,
) -> anyhow::Result<BootstrapSessionReader> {
    let reader = BootstrapSessionReader::new(ports.clone(), edge).with_labels(labels);
    let state = match saved.get(&reader.edge().producer) {
        Some(Handover::Provider(state)) => state.clone(),
        _ => None,
    };
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
        let mut saved = saved_handovers(state).await?;
        let mut replay = Vec::new();
        if self.bootstrap_complete() {
            let preparation = self
                .resume_producers(state, &mut saved, &mut replay)
                .await?;
            if preparation != BootstrapPreparation::Ready {
                return Ok(preparation);
            }
        }
        *lock(&self.prepared) = Some(Prepared {
            persistent,
            saved,
            replay,
            collected: Arc::default(),
        });
        Ok(BootstrapPreparation::Ready)
    }

    async fn prepare_gap_rebuild(
        &self,
        state: &dyn BootstrapState,
        binding: &InputBinding,
        requested: u64,
    ) -> anyhow::Result<Option<OutputCursor>> {
        let inputs = self.ports.inputs();
        let [edge] = inputs.as_slice() else {
            return Ok(None);
        };
        if edge.local
            || !persistent(state)
            || self.ports.outputs().is_empty()
            || edge.producer != binding.output
            || !self.bootstrap_complete()
        {
            return Ok(None);
        }
        let saved = saved_handovers(state).await?;
        let Some(Handover::Output { producer }) = saved.get(&binding.output) else {
            return Ok(None);
        };
        anyhow::ensure!(
            saved.len() == 1
                && producer.persistent()
                && producer.graph_id() == self.graph_id.as_ref()
                && producer.component_id() == &binding.output.component
                && producer.stream() == &binding.stream,
            "rebuild does not match the installed graph output"
        );
        let after = OutputCursor::try_new(producer.clone(), requested)?;
        *lock(&self.prepared) = Some(Prepared {
            persistent: true,
            saved,
            replay: Vec::new(),
            collected: Arc::default(),
        });
        Ok(Some(after))
    }

    async fn prepare_first_input_rebuild(
        &self,
        state: &dyn BootstrapState,
        binding: &InputBinding,
    ) -> anyhow::Result<bool> {
        let inputs = self.ports.inputs();
        let [edge] = inputs.as_slice() else {
            return Ok(false);
        };
        let Some(progress) = &self.progress else {
            return Ok(false);
        };
        // Completed, persistent, and never read: an installed view of no input.
        if edge.local
            || !persistent(state)
            || self.ports.outputs().is_empty()
            || edge.producer != binding.output
            || !self.bootstrap_complete()
            || !progress.snapshot().checkpoints.is_empty()
            || !saved_handovers(state).await?.is_empty()
        {
            return Ok(false);
        }
        *lock(&self.prepared) = Some(Prepared {
            persistent: true,
            saved: Handovers::new(),
            replay: Vec::new(),
            collected: Arc::default(),
        });
        Ok(true)
    }

    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("graph bootstrap requires the consumer's bootstrap state")
    }

    async fn open_snapshot(
        &self,
        state: Arc<dyn BootstrapState>,
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
        let mut reader = open_reader(
            &self.ports,
            &self.control,
            persistent,
            &saved,
            first,
            self.labels.clone(),
        )
        .await?;
        let mut retained = saved;
        let first = loop {
            match reader.next().await? {
                BootstrapSessionItem::State(intent) => {
                    retain_intent(
                        state.as_ref(),
                        &mut retained,
                        &reader.edge().producer,
                        intent,
                    )
                    .await?;
                }
                item => break item,
            }
        };
        let readers: std::collections::VecDeque<_> = [(reader, Some(first))].into();
        let graph_id = self.graph_id.clone();
        let ports = self.ports.clone();
        let control = self.control.clone();
        let labels = self.labels.clone();
        let changes = futures::stream::try_unfold(
            (readers, pending, collected, retained),
            move |(mut readers, mut pending, collected, mut retained)| {
                let graph_id = graph_id.clone();
                let ports = ports.clone();
                let control = control.clone();
                let state = state.clone();
                let labels = labels.clone();
                async move {
                    loop {
                        if readers.is_empty() {
                            let Some(edge) = pending.pop_front() else {
                                lock(&collected).complete = true;
                                return Ok(None);
                            };
                            let reader = open_reader(
                                &ports,
                                &control,
                                persistent,
                                &retained,
                                edge,
                                labels.clone(),
                            )
                            .await?;
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
                            BootstrapSessionItem::State(intent) => {
                                retain_intent(
                                    state.as_ref(),
                                    &mut retained,
                                    &reader.edge().producer,
                                    intent,
                                )
                                .await?;
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
                                return Ok(Some((record, (readers, pending, collected, retained))));
                            }
                            BootstrapSessionItem::Complete { watermarks, state } => {
                                let producer = reader.edge().producer.clone();
                                readers.pop_front();
                                let mut collected = lock(&collected);
                                collected.watermarks.extend(watermarks);
                                if persistent {
                                    collected.states.insert(producer, Handover::Provider(state));
                                }
                            }
                            BootstrapSessionItem::OutputComplete(cut) => {
                                let endpoint = reader.edge().producer.clone();
                                anyhow::ensure!(
                                    cut.producer().component_id() == &endpoint.component,
                                    "snapshot cut belongs to another producer"
                                );
                                readers.pop_front();
                                let mut collected = lock(&collected);
                                collected
                                    .watermarks
                                    .push(BootstrapWatermark::from_output(&cut));
                                if persistent {
                                    anyhow::ensure!(
                                        cut.producer().persistent(),
                                        "persistent bootstrap from a volatile output"
                                    );
                                    anyhow::ensure!(
                                        cut.producer().graph_id() == graph_id.as_ref(),
                                        "persistent snapshot cut belongs to a different graph"
                                    );
                                    collected.states.insert(
                                        endpoint,
                                        Handover::Output {
                                            producer: cut.producer().clone(),
                                        },
                                    );
                                }
                            }
                            BootstrapSessionItem::Replay(_) => {
                                anyhow::bail!("unexpected retained envelope in snapshot")
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
        self.ports.check_windows()?;
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
        Ok(Some(encode_sessions(&collected.states)?))
    }

    fn take_input_replay(&self) -> anyhow::Result<Vec<ChangeEnvelope>> {
        let mut prepared = lock(&self.prepared);
        let prepared = prepared
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("graph bootstrap was not prepared"))?;
        Ok(std::mem::take(&mut prepared.replay))
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

struct ProviderScan {
    provider: Arc<dyn ComputationBootstrapProvider>,
    worker: tokio::sync::RwLock<Option<tokio::task::JoinHandle<anyhow::Result<()>>>>,
}

impl ProviderScan {
    // Borrow the join in its slot. Dropping a control callback or timing out
    // the lifecycle waiter transfers neither the worker nor the provider.
    async fn join(slot: &mut Option<Self>) -> anyhow::Result<()> {
        if let Some(scan) = slot {
            let worker = scan.worker.get_mut();
            if let Some(join) = worker.as_mut() {
                let result = join.await;
                *worker = None;
                result??;
            } else {
                scan.provider.stop().await?;
            }
            *slot = None;
        }
        Ok(())
    }
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
    active: tokio::sync::Mutex<Option<ProviderScan>>,
    read_scans: tokio::sync::Mutex<Vec<Option<ProviderScan>>>,
    state: tokio::sync::watch::Sender<ProviderState>,
}

#[async_trait]
impl IndependentStateProvider for ProviderServer {
    async fn open_state(
        &self,
        represented: &OutputEndpoint,
        input_port: &PortId,
        provider_port: &PortId,
    ) -> std::result::Result<StateScan, OutputReadError> {
        const MAX_READS: usize = 32;
        let (rows, mut receive) = tokio::sync::mpsc::channel(1);
        let (terminal, finished) = tokio::sync::oneshot::channel();
        {
            let mut scans = self.read_scans.lock().await;
            for slot in scans.iter_mut() {
                if slot.as_mut().is_some_and(|s| {
                    s.worker
                        .get_mut()
                        .as_ref()
                        .is_none_or(tokio::task::JoinHandle::is_finished)
                }) {
                    ProviderScan::join(slot)
                        .await
                        .map_err(OutputReadError::Failed)?;
                }
            }
            scans.retain(Option::is_some);
            if scans.len() == MAX_READS {
                return Err(OutputReadError::NotReady(
                    "provider has no free current-state read slot".into(),
                ));
            }
            let mut state = self.state.subscribe();
            if *state.borrow() != ProviderState::Running {
                return Err(OutputReadError::NotReady(
                    "current-state provider is not running".into(),
                ));
            }
            let request = BootstrapRequest {
                session: BootstrapSessionId::new(),
                consumer: represented.component.clone(),
                input_port: input_port.clone(),
                output_port: provider_port.clone(),
                persistent: false,
                state: None,
                origin: None,
                resume: false,
                replay_after: None,
                labels: None,
            };
            let provider = (self.factory)(&request).map_err(OutputReadError::Failed)?;
            let scan = ProviderScan {
                provider: provider.clone(),
                worker: Default::default(),
            };
            let work = async move {
                let result = tokio::select! {
                    biased;
                    _ = rows.closed() => Ok(()),
                    _ = state.wait_for(|s| *s != ProviderState::Running) => Err(OutputReadError::Retired),
                    result = async {
                        anyhow::ensure!(provider.recovery_reader().is_none(), "independent scan cannot bind consumer progress");
                        anyhow::ensure!(provider.prepare().await? == BootstrapPreparation::Ready, "independent scan requires fresh read-only preparation");
                        let mut view = provider.snapshot().await?;
                        anyhow::ensure!(view.watermarks.is_empty(), "independent scan attempted a handover");
                        while let Some(row) = view.changes.next().await {
                            rows.send(row?).await.map_err(|_| anyhow::anyhow!("current-state reader closed"))?;
                        }
                        drop(view);
                        anyhow::ensure!(provider.complete_snapshot().await?.is_empty() && provider.completion_state()?.is_none(), "independent scan returned handover state");
                        anyhow::Ok(())
                    } => result.map_err(OutputReadError::Failed),
                };
                drop(rows);
                // A closed queue is not proof of success. Return EOF only
                // after the scan and asynchronous connector cleanup finished.
                let stopped = provider.stop().await;
                let message = match &stopped {
                    Ok(()) => result,
                    Err(e) => Err(OutputReadError::Failed(anyhow::anyhow!(
                        "current-state provider cleanup: {e:#}"
                    ))),
                };
                let _ = terminal.send(message);
                stopped
            };
            crate::context::workers::try_spawn_owned_worker(
                &scan.worker,
                crate::observability::ExecutionScope::capture().scope(work),
            )
            .map_err(|e| OutputReadError::Failed(e.into()))?;
            scans.push(Some(scan));
        }
        // Some connectors establish their read transaction on the first page.
        // Do not return an unopened read that can silently capture a later view.
        let Some(first) = receive.recv().await else {
            finished
                .await
                .map_err(|e| OutputReadError::Failed(e.into()))??;
            return Ok(futures::stream::empty().boxed());
        };
        let rest = futures::stream::try_unfold((receive, finished), |(mut rx, end)| async move {
            match rx.recv().await {
                Some(row) => Ok(Some((row, (rx, end)))),
                None => {
                    end.await.map_err(|e| OutputReadError::Failed(e.into()))??;
                    Ok(None)
                }
            }
        });
        Ok(futures::stream::once(async move { Ok(first) })
            .chain(rest)
            .boxed())
    }
}

impl ProviderServer {
    async fn serve(&self, request: &BootstrapRequest) -> anyhow::Result<()> {
        let ports = lock(&self.ports).clone();
        let mut reply = BootstrapReply::open(&ports, request)?;
        let mut state = self.state.subscribe();
        let ready = tokio::select! {
            biased;
            _ = reply.lifetime.clone().ended() => return Ok(()),
            ready = state.wait_for(|state| *state != ProviderState::Idle) => ready.map(|value| *value),
        };
        if !matches!(ready, Ok(ProviderState::Running)) {
            return reply.fail("bootstrap provider is disposed").await;
        }
        let mut active = self.active.lock().await;
        if let Err(error) = ProviderScan::join(&mut active).await {
            return reply.finish(Err(error)).await;
        }
        if *state.borrow() != ProviderState::Running {
            return reply.fail("bootstrap provider stopped").await;
        }
        let provider = match (self.factory)(request) {
            Ok(provider) => provider,
            Err(error) => {
                reply.begin().await?;
                return reply.finish(Err(error)).await;
            }
        };
        let scan = active.insert(ProviderScan {
            provider: provider.clone(),
            worker: Default::default(),
        });
        let request = request.clone();
        let work = async move {
            let stopping = async {
                let _ = state
                    .wait_for(|state| *state != ProviderState::Running)
                    .await;
            };
            if let Err(error) =
                run_provider(&mut reply, &request, provider.as_ref(), stopping).await?
            {
                log::warn!(
                    "Bootstrap session {} for {} failed: {error:#}",
                    request.session,
                    request.consumer
                );
            }
            Ok(())
        };
        crate::context::workers::try_spawn_owned_worker(
            &scan.worker,
            crate::observability::ExecutionScope::capture().scope(work),
        )?;
        ProviderScan::join(&mut active).await
    }

    async fn settle(&self) -> anyhow::Result<()> {
        tokio::time::timeout(STOP_GRACE, async {
            ProviderScan::join(&mut *self.active.lock().await).await?;
            let mut scans = self.read_scans.lock().await;
            for slot in scans.iter_mut() {
                ProviderScan::join(slot).await?;
            }
            scans.clear();
            anyhow::Ok(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("bootstrap provider cleanup timed out; scan is still owned"))?
    }

    async fn cancel(&self, state: ProviderState) -> anyhow::Result<()> {
        self.state.send_replace(state);
        self.settle().await
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
    description: super::ComponentDescription,
    server: Arc<ProviderServer>,
    readers: OutputReaders,
}

impl BootstrapProviderComponent {
    /// The default provider node description: the node keeps no progress of
    /// its own, its bootstrap outputs declare no producer identity (each
    /// serves a requesting consumer's own session), and its bootstrap facts
    /// are undeclared. Providers that know their facts declare them with
    /// [`Self::with_description`] (see [`Self::declared_description`]).
    pub fn provider_description() -> super::ComponentDescription {
        super::ComponentDescription::new(super::ProgressPersistence::Stateless)
    }

    /// A provider node's description declaring `output` on every bootstrap
    /// output of `descriptor`.
    pub fn declared_description(
        descriptor: &ComponentDescriptor,
        output: drasi_computation_contracts::BootstrapOutputDescription,
    ) -> super::ComponentDescription {
        descriptor
            .ports()
            .iter()
            .filter(|port| {
                port.kind() == PortKind::Bootstrap && port.direction() == PortDirection::Output
            })
            .fold(Self::provider_description(), |description, port| {
                description.with_port(
                    port.id().clone(),
                    super::PortDescription::BootstrapOutput(output.clone()),
                )
            })
    }

    /// Replace the node's description, such as with its factory's declared
    /// bootstrap facts.
    pub fn with_description(mut self, description: super::ComponentDescription) -> Self {
        self.description = description;
        self
    }

    /// Opt in only for a factory that opens a fresh, effect-free point-in-time
    /// reader per call. Never reuse a paired source handover for inspection.
    pub fn with_independent_read(mut self, port: PortId) -> anyhow::Result<Self> {
        let output = self.description.bootstrap_output(&port);
        anyhow::ensure!(
            self.descriptor.ports().iter().any(|p| p.id() == &port)
                && output.completion == BootstrapCompletionKind::Quiescent
                && output.pairing.is_none(),
            "independent reads require a declared quiescent provider output"
        );
        self.readers = self
            .readers
            .with_independent_provider(port, self.server.clone());
        Ok(self)
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
            description: Self::provider_description(),
            readers: OutputReaders::default(),
            server: Arc::new(ProviderServer {
                ports: Mutex::new(BootstrapPorts::default()),
                factory,
                active: Default::default(),
                read_scans: Default::default(),
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
    fn output_readers(&self) -> OutputReaders {
        self.readers.clone()
    }

    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn description(&self) -> super::ComponentDescription {
        self.description.clone()
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
        self.server.settle().await?;
        self.server.state.send_replace(ProviderState::Running);
        Ok(())
    }

    async fn stop(&mut self, _: &StopContext) -> anyhow::Result<()> {
        self.server.cancel(ProviderState::Idle).await
    }

    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        self.server.cancel(ProviderState::Disposed).await?;
        anyhow::ensure!(
            !context.erases_owned_state(),
            "a bootstrap provider owns no state to remove"
        );
        Ok(())
    }
}

impl ComputationBootstrapper for BootstrapProviderComponent {}

/// Registers the live boundary for a seed that supplies no position of its
/// own. A paired reader's watermarks take precedence. Never sample the live
/// head after the provider has scanned the datastore.
pub type BootstrapHandover =
    Arc<dyn Fn(&BootstrapRequest) -> anyhow::Result<Vec<BootstrapWatermark>> + Send + Sync>;

/// A source's bootstrap relay. It serves each downstream session from the
/// provider on the source's bootstrap input: one upstream session per
/// downstream session, forwarding records and failures, and completing with
/// the source's handover watermarks and the provider's completion state.
/// Without a bootstrap input the snapshot is unavailable. Use it as the source's
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
        let abandoned = reply.lifetime.clone().ended();
        reply.begin().await?;
        let read = async {
            anyhow::ensure!(
                request.replay_after.is_none(),
                "a source handover cannot replay query output"
            );
            let inputs = ports.inputs();
            anyhow::ensure!(
                inputs.len() <= 1,
                "a source has at most one bootstrap provider"
            );
            let edge = inputs.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("current state is unavailable: no bootstrap provider is bound")
            })?;
            let seed_cut = if request.resume {
                Vec::new()
            } else {
                (self.handover)(request)?
            };
            // The relay forwards its origin's declared labels unchanged.
            let mut upstream = BootstrapSessionReader::new(ports.clone(), edge)
                .with_labels(request.labels.clone());
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
                    BootstrapSessionItem::State(intent) => {
                        reply.state(intent.state().clone()).await?;
                        intent.acknowledge()?;
                    }
                    // A resumed consumer keeps its live position; there is no
                    // new handover boundary.
                    BootstrapSessionItem::Complete { state, .. } if request.resume => {
                        return Ok((Vec::new(), state))
                    }
                    BootstrapSessionItem::Complete { watermarks, state } => {
                        let cut = if watermarks.is_empty() {
                            seed_cut
                        } else {
                            watermarks
                        };
                        return Ok((cut, state));
                    }
                    BootstrapSessionItem::Replay(_) | BootstrapSessionItem::OutputComplete(_) => {
                        anyhow::bail!("expected a source bootstrap provider, not an output reader")
                    }
                }
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = abandoned => return Ok(()),
            outcome = read => outcome,
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct HeldScan {
        reading: tokio::sync::Notify,
        stopping: tokio::sync::Notify,
        release_cleanup: tokio::sync::Notify,
    }

    #[async_trait]
    impl ComputationBootstrapProvider for HeldScan {
        async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
            self.reading.notify_one();
            std::future::pending().await
        }
        async fn stop(&self) -> anyhow::Result<()> {
            self.stopping.notify_one();
            self.release_cleanup.notified().await;
            Ok(())
        }
    }

    async fn interrupt_provider(replace: bool) -> anyhow::Result<()> {
        let producer = crate::test_helpers::component_id(1);
        let consumer = crate::test_helpers::component_id(2);
        let scan = Arc::new(HeldScan::default());
        let node = BootstrapProviderComponent::shared(producer.clone(), scan.clone())?;
        let ports = lock(&node.server.ports).clone();
        let input = BootstrapInputEdge {
            producer: OutputEndpoint::new(producer, PortId::try_new("snapshot")?),
            input_port: PortId::try_new("bootstrap")?,
            local: true,
        };
        let mut pipe = BoundedPipe::new(2)?;
        ports.bind_session_pipe(
            &ports,
            0,
            BootstrapOutputEdge {
                output_port: input.producer.port.clone(),
                consumer: consumer.clone(),
                input_port: input.input_port.clone(),
            },
            input.clone(),
            (pipe.sender(), pipe.take_receiver()?),
            None,
        );
        let mut reader = BootstrapSessionReader::new(ports.clone(), input.clone());
        let request = BootstrapRequest {
            session: reader.session(),
            consumer,
            input_port: input.input_port,
            output_port: input.producer.port,
            persistent: false,
            state: None,
            origin: None,
            resume: false,
            replay_after: None,
            labels: None,
        };
        node.server.state.send_replace(ProviderState::Running);
        let mut callback = Box::pin(node.server.serve(&request));
        tokio::select! {
            _ = scan.reading.notified() => (),
            result = &mut callback => panic!("scan did not stay active: {result:?}"),
        }
        // Reconciliation can drop the control callback before retiring the edge.
        drop(callback);
        if replace {
            ports.remove_edge(0);
            // No stop request to the node yet: losing this binding alone must
            // cancel its external scan.
            scan.stopping.notified().await;
        }
        let mut stop = Box::pin(node.server.cancel(ProviderState::Idle));
        if !replace {
            tokio::select! {
                _ = scan.stopping.notified() => (),
                result = &mut stop => panic!("reported stopped before provider cleanup: {result:?}"),
            }
        }
        assert!(futures::poll!(&mut stop).is_pending());
        drop(stop);
        scan.release_cleanup.notify_one();
        node.server.cancel(ProviderState::Idle).await?;
        if !replace {
            let error = reader.next().await.unwrap_err();
            assert!(
                error.to_string().contains("bootstrap provider stopped"),
                "{error}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn stopping_a_provider_joins_its_paused_scan_after_caller_cancellation(
    ) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(3), interrupt_provider(false)).await?
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retiring_a_binding_releases_the_scan_even_after_its_callback_is_dropped(
    ) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(3), interrupt_provider(true)).await?
    }

    #[tokio::test]
    async fn abandoned_session_cannot_consume_a_replacement_bindings_reply() -> anyhow::Result<()> {
        let ports = BootstrapPorts::default();
        let edge = BootstrapInputEdge {
            producer: OutputEndpoint::new(
                crate::test_helpers::component_id(1),
                PortId::try_new("out")?,
            ),
            input_port: PortId::try_new("bootstrap")?,
            local: false,
        };
        let bind = |pipe: &mut BoundedPipe| -> anyhow::Result<()> {
            ports.bind_session_pipe(
                &ports,
                0,
                BootstrapOutputEdge {
                    output_port: edge.producer.port.clone(),
                    consumer: crate::test_helpers::component_id(2),
                    input_port: edge.input_port.clone(),
                },
                edge.clone(),
                (pipe.sender(), pipe.take_receiver()?),
                None,
            );
            Ok(())
        };
        let mut old = BoundedPipe::new(2)?;
        bind(&mut old)?;
        let mut abandoned = BootstrapSessionReader::new(ports.clone(), edge.clone());
        ports.remove_edge(0);
        let mut new = BoundedPipe::new(2)?;
        bind(&mut new)?;
        let mut active = BootstrapSessionReader::new(ports, edge);
        for (sequence, event) in [
            BootstrapSessionEvent::Begin,
            BootstrapSessionEvent::Complete {
                watermarks: Vec::new(),
                state: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let frame = BootstrapMarker::new(active.session(), event).envelope(
                StreamId::try_new("test/session")?,
                sequence as u64 + 1,
                GraphChangeCodec::schema().descriptor().clone(),
            )?;
            new.sender()
                .send(frame)
                .await
                .map_err(|failure| failure.error)?;
        }
        let result = tokio::time::timeout(Duration::from_secs(2), abandoned.next()).await?;
        assert!(result.unwrap_err().to_string().contains("replaced"));
        assert!(matches!(
            active.next().await?,
            BootstrapSessionItem::Complete { .. }
        ));
        Ok(())
    }

    struct ExpiredHistory(OutputCursor);

    #[async_trait]
    impl SnapshotProvider for ExpiredHistory {
        async fn snapshot(&self) -> std::result::Result<OutputSnapshot, OutputReadError> {
            panic!("a completed consumer must not rescan")
        }
    }

    #[async_trait]
    impl ReplayProvider for ExpiredHistory {
        async fn committed_head(&self) -> std::result::Result<OutputCursor, OutputReadError> {
            Ok(self.0.clone())
        }

        async fn replay_after(
            &self,
            after: &OutputCursor,
        ) -> std::result::Result<OutputReplay, OutputReadError> {
            after.ensure_same_epoch(&self.0)?;
            Err(OutputReadError::Gap {
                requested: after.sequence(),
                oldest: Some(9),
                latest: self.0.sequence(),
            })
        }
    }

    #[tokio::test]
    async fn output_replay_reports_typed_gap_and_epoch_change_across_the_edge() -> anyhow::Result<()>
    {
        let producer = crate::test_helpers::component_id(1);
        let consumer = crate::test_helpers::component_id(2);
        let cursor = |epoch, sequence| -> anyhow::Result<_> {
            Ok(OutputCursor::try_new(
                GraphProducerIdentity::for_epoch(
                    "scope".into(),
                    "graph".into(),
                    producer.clone(),
                    StreamId::try_new("upstream/out")?,
                    uuid::Uuid::from_u128(epoch),
                    true,
                )?,
                sequence,
            )?)
        };
        let current = cursor(1, 10)?;
        let history = Arc::new(ExpiredHistory(current.clone()));
        let server = BootstrapSnapshotServer::new(history.clone(), Some(history));
        let ports = BootstrapPorts::default();
        let input = BootstrapInputEdge {
            producer: OutputEndpoint::new(producer.clone(), PortId::try_new("snapshot")?),
            input_port: PortId::try_new("bootstrap")?,
            local: false,
        };
        let mut pipe = BoundedPipe::new(2)?;
        ports.bind_session_pipe(
            &ports,
            0,
            BootstrapOutputEdge {
                output_port: input.producer.port.clone(),
                consumer: consumer.clone(),
                input_port: input.input_port.clone(),
            },
            input.clone(),
            (pipe.sender(), pipe.take_receiver()?),
            None,
        );
        server.bind(ports.clone());
        for epoch in [1, 2] {
            let after = cursor(epoch, 5)?;
            let mut reader = BootstrapSessionReader::new(ports.clone(), input.clone());
            let request = BootstrapRequest {
                session: reader.session(),
                consumer: consumer.clone(),
                input_port: input.input_port.clone(),
                output_port: input.producer.port.clone(),
                persistent: true,
                state: None,
                origin: None,
                resume: false,
                replay_after: Some(Box::new(after.clone())),
                labels: None,
            };
            let (sent, received) = tokio::join!(server.serve(&request), reader.next());
            assert!(sent.is_err());
            let error = received.unwrap_err();
            match (epoch, error.downcast_ref::<OutputReadError>()) {
                (
                    1,
                    Some(OutputReadError::Gap {
                        requested: 5,
                        oldest: Some(9),
                        latest: 10,
                    }),
                ) => {}
                (
                    2,
                    Some(OutputReadError::EpochChanged {
                        requested,
                        current: actual,
                    }),
                ) => {
                    assert_eq!(requested.as_ref(), after.producer());
                    assert_eq!(actual.as_ref(), current.producer());
                }
                _ => panic!("lost typed replay error: {error:#}"),
            }
        }
        Ok(())
    }
}
