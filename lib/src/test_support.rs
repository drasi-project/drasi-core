// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Per-query deterministic timing on the native ComputationGraph runtime.
//!
//! Attach using [`crate::DrasiLibBuilder::with_query_test_control`]. No global
//! clock, source timestamps, query configuration or production cadence changes.
//! See `lib/docs/query-test-control.md` for ordering and recovery scope.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify, OwnedMutexGuard};

use crate::{
    computation::v1::{
        ComputationInspection, ComputationInspector, GraphError, PublicationObserver,
        QueryPublicationMode,
    },
    error::{DrasiError, Result},
};

/// Completed due processing and output handoff for one drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    /// Physical eligibility time in epoch milliseconds; not a source watermark.
    pub physical_time_ms: u64,
    /// Committed query result sequence, not replay transport sequence.
    ///
    /// Output produced/replayed by this drain has been accepted by the query's
    /// immediate outgoing pipes and its delivery-confirmation hook has succeeded.
    /// This is not a historical delivery ledger across interrupted runs, nor a
    /// fence for outlet subscribers, reactions or external effects.
    pub output_sequence: u64,
    /// Reset generation to which `output_sequence` belongs.
    pub output_generation: u64,
    /// Whether the configured query actually persists its output.
    pub output_persistent: bool,
    /// The query's actual publication mode; the control does not upgrade it.
    pub publication: QueryPublicationMode,
}

/// An explicit physical eligibility clock and awaited native query frontier.
///
/// One control binds to one builder query for its lifetime. Clones share time and
/// serialization. Stopping/restarting that query retains the clock, but rebuilding
/// a library requires a new control. This control is not persisted.
#[derive(Clone)]
pub struct QueryTestControl {
    inner: Arc<ControlInner>,
}

struct ControlInner {
    now: AtomicU64,
    bound: AtomicBool,
    next_request: AtomicU64,
    operation: Arc<AsyncMutex<()>>,
    state: Mutex<ControlState>,
    queued: AtomicBool,
    changed: Notify,
}

#[derive(Default)]
struct ControlState {
    endpoint: Option<mpsc::Sender<u64>>,
    pending: HashMap<u64, PendingRequest>,
    failure: Option<Arc<GraphError>>,
}

impl QueryTestControl {
    /// Create an unbound clock at an absolute epoch-millisecond time.
    pub fn new(now_ms: u64) -> Self {
        Self {
            inner: Arc::new(ControlInner {
                now: AtomicU64::new(now_ms),
                bound: AtomicBool::new(false),
                next_request: AtomicU64::new(0),
                operation: Arc::new(AsyncMutex::new(())),
                state: Mutex::new(ControlState::default()),
                queued: AtomicBool::new(false),
                changed: Notify::new(),
            }),
        }
    }

    pub fn now(&self) -> u64 {
        self.inner.now.load(Ordering::Acquire)
    }

    /// Advance monotonically and finish currently due work without a source event.
    ///
    /// The query rechecks its real future queue between transactions, using its
    /// unchanged logical clocks and stale-hint validation. Success requires no
    /// remaining due work, committed query output and immediate pipe acceptance.
    /// It does NOT flush ingress or wait for downstream handling. Establish
    /// query-applied input ordering separately; a source WAL position is not that
    /// boundary.
    ///
    /// Timeout covers serialization and backpressure. Timeout or caller
    /// cancellation does not undo time/work or release serialization: the graph
    /// owns the request until completion, failure or stop. Stop/shutdown the
    /// library to cancel blocked processing, and await that lifecycle operation.
    pub async fn advance_to(&self, now_ms: u64, timeout: Duration) -> Result<DrainReport> {
        self.request(Some(now_ms), timeout).await
    }

    /// Drain at unchanged physical time, including an empty or not-yet-due queue.
    pub async fn wake(&self, timeout: Duration) -> Result<DrainReport> {
        self.request(None, timeout).await
    }

    async fn request(&self, now_ms: Option<u64>, timeout: Duration) -> Result<DrainReport> {
        tokio::time::timeout(timeout, async {
            let operation = self.inner.operation.clone().lock_owned().await;
            let endpoint = {
                let state = self.inner.state.lock().expect("test control state");
                state.endpoint.clone().ok_or_else(|| unavailable(&state))?
            };
            let permit = endpoint
                .clone()
                .reserve_owned()
                .await
                .map_err(|_| stopped())?;
            let rx = {
                let mut state = self.inner.state.lock().expect("test control state");
                if !state
                    .endpoint
                    .as_ref()
                    .is_some_and(|active| active.same_channel(&endpoint))
                {
                    return Err(unavailable(&state));
                }
                let now = now_ms.unwrap_or_else(|| self.now());
                if now < self.now() {
                    return Err(DrasiError::invalid_config(
                        "test physical clock cannot move backwards",
                    ));
                }
                if i64::try_from(now)
                    .ok()
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .is_none()
                {
                    return Err(DrasiError::invalid_config(
                        "test physical clock is out of range",
                    ));
                }
                let id = self
                    .inner
                    .next_request
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
                    .map_err(|_| DrasiError::invalid_state("test request IDs exhausted"))?;
                let (tx, rx) = oneshot::channel();
                state.pending.insert(
                    id,
                    PendingRequest {
                        tx,
                        _operation: operation,
                    },
                );
                self.inner.now.store(now, Ordering::Release);
                permit.send(id);
                self.inner.queued.store(true, Ordering::Release);
                self.inner.changed.notify_one();
                rx
            };
            rx.await.map_err(|_| stopped())?
        })
        .await
        .map_err(|_| {
            DrasiError::operation_failed(
                "query",
                "test-control",
                "drain",
                "timed out waiting for the native query output frontier",
            )
        })?
    }

    pub(crate) fn bind(&self) -> Result<()> {
        self.inner
            .bound
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| {
                DrasiError::invalid_config("a QueryTestControl can bind to only one query")
            })
    }

    pub(crate) fn observe(&self, inspector: &ComputationInspector) -> Arc<dyn PublicationObserver> {
        let observer: Arc<dyn PublicationObserver> =
            Arc::new(FailureObserver(Arc::downgrade(&self.inner)));
        inspector.observe(&observer);
        observer
    }

    pub(crate) fn connect(&self) -> Result<QueryTestSession> {
        let mut state = self.inner.state.lock().expect("test control state");
        if state.endpoint.is_some() {
            return Err(DrasiError::invalid_state(
                "controlled query is already started",
            ));
        }
        let (tx, rx) = mpsc::channel(1);
        state.endpoint = Some(tx);
        state.failure = None;
        self.inner.queued.store(false, Ordering::Release);
        Ok(QueryTestSession {
            control: self.clone(),
            rx,
            active: None,
        })
    }

    pub(crate) async fn notified(&self) {
        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.queued.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

fn stopped() -> DrasiError {
    DrasiError::invalid_state("controlled query is not started or stopped before drain completion")
}

fn unavailable(state: &ControlState) -> DrasiError {
    match &state.failure {
        Some(cause) => {
            DrasiError::operation_failed("query", "test-control", "drain", format!("{cause:#}"))
                .with_cause(GraphError::Reported {
                    cause: cause.clone(),
                })
        }
        None => stopped(),
    }
}

struct PendingRequest {
    tx: oneshot::Sender<Result<DrainReport>>,
    _operation: OwnedMutexGuard<()>,
}

struct FailureObserver(Weak<ControlInner>);

impl PublicationObserver for FailureObserver {
    fn publish(&self, previous: &ComputationInspection, current: &ComputationInspection) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        // This observer belongs to the ordinary query's private QueryGraph,
        // including its scheduler, source adapters and output outlet, not siblings.
        if let Some(failure) = current.observed.components.iter().find_map(|(id, node)| {
            let failure = node.failure.as_ref()?;
            let unchanged = previous
                .observed
                .components
                .get(id)
                .and_then(|node| node.failure.as_ref())
                .is_some_and(|old| Arc::ptr_eq(&old.cause, &failure.cause));
            (!unchanged).then_some(failure)
        }) {
            let mut state = inner.state.lock().expect("test control state");
            state.failure.get_or_insert_with(|| failure.cause.clone());
            state.endpoint = None;
            for (_, request) in std::mem::take(&mut state.pending) {
                let _ = request.tx.send(Err(unavailable(&state)));
            }
        }
    }
}

pub(crate) struct QueryTestSession {
    pub(crate) control: QueryTestControl,
    rx: mpsc::Receiver<u64>,
    active: Option<u64>,
}

impl QueryTestSession {
    pub(crate) fn begin(&mut self) -> bool {
        if self.active.is_none() {
            // Serialize dequeue/notification state with enqueue, including when
            // a replay continuation or another scheduled hint reaches us first.
            let _state = self.control.inner.state.lock().expect("test control state");
            if let Ok(id) = self.rx.try_recv() {
                self.control.inner.queued.store(false, Ordering::Release);
                self.active = Some(id);
            }
        }
        self.active.is_some()
    }

    pub(crate) fn complete(&mut self, report: DrainReport) {
        if let Some(id) = self.active.take() {
            let pending = self
                .control
                .inner
                .state
                .lock()
                .expect("test control state")
                .pending
                .remove(&id);
            if let Some(pending) = pending {
                let _ = pending.tx.send(Ok(report));
            }
        }
    }
}

impl Drop for QueryTestSession {
    fn drop(&mut self) {
        let mut state = self.control.inner.state.lock().expect("test control state");
        state.endpoint = None;
        for (_, request) in std::mem::take(&mut state.pending) {
            let _ = request.tx.send(Err(unavailable(&state)));
        }
    }
}
