// Copyright 2026 The Drasi Authors.
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

//! Opt-in, per-query integration-test controls. No global clock is changed.
//!
//! Attach a control using [`crate::DrasiLibBuilder::with_query_test_control`].
//! See `lib/timers.md` for source/reaction ordering and recovery limitations.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, OwnedMutexGuard};

use crate::error::{DrasiError, Result};

/// Completion of one manager future drain, not acknowledgement of a wake alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    /// Physical eligibility clock, in milliseconds since the Unix epoch.
    pub physical_time_ms: u64,
    /// Query output sequence after the drain's output-dispatch stage completed.
    pub output_sequence: u64,
}

/// Explicit physical clock and wake/drain fence for one query in one library.
///
/// Clones share the same control. A control can be bound to only one query;
/// create independent controls for parallel instances (even with identical IDs).
/// Uncontrolled queries retain the system clock and normal polling.
#[derive(Clone)]
pub struct QueryTestControl {
    inner: Arc<ControlInner>,
}

struct ControlInner {
    now: AtomicU64,
    bound: AtomicBool,
    next_request: AtomicU64,
    operation: Arc<AsyncMutex<()>>,
    endpoint: Mutex<Option<mpsc::Sender<u64>>>,
    pending: Mutex<HashMap<u64, PendingRequest>>,
}

impl QueryTestControl {
    /// Create an unbound control at an absolute epoch-millisecond physical time.
    pub fn new(now_ms: u64) -> Self {
        Self {
            inner: Arc::new(ControlInner {
                now: AtomicU64::new(now_ms),
                bound: AtomicBool::new(false),
                next_request: AtomicU64::new(0),
                operation: Arc::new(AsyncMutex::new(())),
                endpoint: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Current physical time. Timer evaluation still uses its scheduled logical time.
    pub fn now(&self) -> u64 {
        self.inner.now.load(Ordering::Acquire)
    }

    /// Advance monotonically, wake the real future source without a source event,
    /// and wait for the manager to finish processing all currently due futures.
    ///
    /// Success means every selected future committed, the queue reported no more
    /// due items, and all resulting output persistence and dispatcher calls
    /// completed successfully. It is NOT a reaction-callback/effect fence or an
    /// atomic durability guarantee. No configured persistent writer means no
    /// persistence guarantee.
    ///
    /// This does not flush source ingress. Await a source's committed-position
    /// hook before advancing time if that event must precede the wake.
    ///
    /// The timeout bounds the entire operation, including concurrent callers and
    /// backpressure. Timeout/cancellation does not undo clock advancement or work
    /// already queued. Subsequent commands cannot change time until that drain
    /// finishes or the source stops. Stop/shutdown the library to cancel blocked
    /// processing.
    pub async fn advance_to(&self, now_ms: u64, timeout: Duration) -> Result<DrainReport> {
        self.request(Some(now_ms), timeout).await
    }

    /// Wake and fence at unchanged physical time, including an empty/not-due queue.
    ///
    /// Use after a reschedule to exercise a stale wake. This is subject to the same
    /// source ordering, publication, timeout and cleanup contract as `advance_to`.
    pub async fn wake(&self, timeout: Duration) -> Result<DrainReport> {
        self.request(None, timeout).await
    }

    async fn request(&self, now_ms: Option<u64>, timeout: Duration) -> Result<DrainReport> {
        tokio::time::timeout(timeout, async {
            let operation = self.inner.operation.clone().lock_owned().await;
            let endpoint = self
                .inner
                .endpoint
                .lock()
                .expect("test control endpoint lock")
                .clone()
                .ok_or_else(|| DrasiError::invalid_state("controlled query is not started"))?;
            let permit = endpoint
                .clone()
                .reserve_owned()
                .await
                .map_err(|_| stopped())?;
            let rx = {
                let active = self
                    .inner
                    .endpoint
                    .lock()
                    .expect("test control endpoint lock");
                if !active
                    .as_ref()
                    .is_some_and(|sender| sender.same_channel(&endpoint))
                {
                    return Err(stopped());
                }
                if let Some(now_ms) = now_ms {
                    if now_ms < self.now() {
                        return Err(DrasiError::invalid_config(
                            "test physical clock cannot move backwards",
                        ));
                    }
                    self.inner.now.store(now_ms, Ordering::Release);
                }
                let id = self
                    .inner
                    .next_request
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
                    .map_err(|_| DrasiError::invalid_state("test wake request IDs exhausted"))?;
                let (tx, rx) = oneshot::channel();
                // The manager, not the waiting caller, owns serialization after
                // enqueue. Cancelling/timing out the caller must not release it.
                self.inner
                    .pending
                    .lock()
                    .expect("test pending lock")
                    .insert(
                        id,
                        PendingRequest {
                            tx,
                            _operation: operation,
                        },
                    );
                permit.send(id);
                rx
            };
            rx.await.map_err(|_| stopped())?
        })
        .await
        .map_err(|_| {
            DrasiError::operation_failed(
                "query",
                "test-control",
                "drain futures",
                "timed out waiting for manager completion",
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

    pub(crate) fn connect(&self) -> Result<WakeReceiver> {
        let mut endpoint = self
            .inner
            .endpoint
            .lock()
            .expect("test control endpoint lock");
        if endpoint.is_some() {
            return Err(DrasiError::invalid_state(
                "controlled future source is already connected",
            ));
        }
        let (tx, rx) = mpsc::channel(1);
        *endpoint = Some(tx);
        Ok(WakeReceiver {
            control: self.clone(),
            rx,
        })
    }

    pub(crate) fn complete(&self, id: u64, result: Result<DrainReport>) {
        if let Some(pending) = self
            .inner
            .pending
            .lock()
            .expect("test pending lock")
            .remove(&id)
        {
            // A timed-out or cancelled test no longer needs its completion.
            let _ = pending.tx.send(result);
        }
    }
}

fn stopped() -> DrasiError {
    DrasiError::invalid_state("controlled future source stopped before drain completion")
}

struct PendingRequest {
    tx: oneshot::Sender<Result<DrainReport>>,
    _operation: OwnedMutexGuard<()>,
}

pub(crate) struct WakeReceiver {
    control: QueryTestControl,
    rx: mpsc::Receiver<u64>,
}

impl WakeReceiver {
    pub(crate) async fn recv(&mut self) -> Option<u64> {
        self.rx.recv().await
    }
}

impl Drop for WakeReceiver {
    fn drop(&mut self) {
        self.control
            .inner
            .endpoint
            .lock()
            .expect("test control endpoint lock")
            .take();
        for (_, pending) in self
            .control
            .inner
            .pending
            .lock()
            .expect("test pending lock")
            .drain()
        {
            let _ = pending.tx.send(Err(stopped()));
        }
    }
}
