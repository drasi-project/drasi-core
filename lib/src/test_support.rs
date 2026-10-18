// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Deterministic physical eligibility clocks for ordinary managed queries.
//!
//! Enable `drasi-lib/test-support` and attach a control with
//! [`crate::DrasiLibBuilder::with_query_test_control`]. Drains use the existing
//! query evaluator, transaction owner and output handoff, without source events,
//! a replacement runtime or changes to logical query time.

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

/// Actual query output frontier after currently eligible scheduled work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    /// Physical eligibility time in epoch milliseconds, not a source watermark.
    pub physical_time_ms: u64,
    /// Committed logical query output sequence, not a replay transport sequence.
    ///
    /// Output produced or replayed by the drain has reached the immediate outgoing
    /// pipes and its delivery-confirmation hook has succeeded. This does not fence
    /// outlet subscribers, reactions, external effects or historical delivery.
    pub output_sequence: u64,
    /// Reset generation to which the output sequence belongs.
    pub output_generation: u64,
    /// Whether this query actually persists its output.
    pub output_persistent: bool,
    /// The query's actual publication contract; controls never upgrade it.
    pub publication: QueryPublicationMode,
}

/// An independent managed-query clock and awaited due-work frontier.
///
/// Clones share time and request serialization. One control binds to one builder
/// query; restarting that query retains the clock. Rebuilding a library requires
/// a new control. Controls are not persisted or part of query/config identities.
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
    /// Create an unbound clock. The builder validates the epoch-millisecond range.
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

    /// Current physical eligibility time, independent of logical event time.
    pub fn now(&self) -> u64 {
        self.inner.now.load(Ordering::Acquire)
    }

    /// Advance monotonically and drain the query's actual currently due work.
    ///
    /// Await query readiness and establish query-applied input ordering first.
    /// Source admission alone is not that boundary. Logical evaluation still uses
    /// the scheduled due time, not this physical clock.
    ///
    /// Timeout includes serialization and output backpressure. Timeout or caller
    /// cancellation does not undo time/work or release graph-owned serialization.
    /// Lifecycle operations retain normal graceful cleanup: release deliberately
    /// blocked output/storage gates and await stop or shutdown. Controls do not
    /// force-cancel work or bypass cleanup.
    pub async fn advance_to(&self, now_ms: u64, timeout: Duration) -> Result<DrainReport> {
        self.request(Some(now_ms), timeout).await
    }

    /// Drain at unchanged time, acknowledging even an empty or not-yet-due queue.
    ///
    /// Stale scheduled entries use the evaluator's normal validation; a wake
    /// never substitutes for, or consumes, a newer not-yet-due generation.
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
                validate_time(now)?;
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
        validate_time(self.now())?;
        self.inner
            .bound
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| {
                DrasiError::invalid_config("a QueryTestControl can bind to only one query")
            })
    }

    pub(crate) fn observe(&self, inspector: &ComputationInspector) -> Arc<dyn PublicationObserver> {
        let observer: Arc<dyn PublicationObserver> = Arc::new(FailureObserver {
            control: Arc::downgrade(&self.inner),
        });
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

fn validate_time(now: u64) -> Result<()> {
    if i64::try_from(now)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .is_none()
    {
        return Err(DrasiError::invalid_config(
            "test physical clock is out of range",
        ));
    }
    Ok(())
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

struct FailureObserver {
    control: Weak<ControlInner>,
}

impl PublicationObserver for FailureObserver {
    fn publish(&self, previous: &ComputationInspection, current: &ComputationInspection) {
        let Some(inner) = self.control.upgrade() else {
            return;
        };
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
            // Serialize notification state with enqueue, even during a replay continuation.
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
