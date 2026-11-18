// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Acknowledged pausing of a component's own workers.
//!
//! The owner holds a [`PauseControl`] and hands each worker a [`PauseGate`].
//! [`PauseControl::pause`] returns only after every live gate has acknowledged
//! the request at a boundary its worker chose, so a successful Pause means no
//! worker is reading, writing or emitting. Admission paths that refuse work
//! while paused, such as a REST handler, observe [`PauseControl::requests`]
//! without acknowledging.
//!
//! Each request carries an epoch. A gate acknowledges a specific epoch, so an
//! acknowledgement left over from an earlier Pause never satisfies a later one.
//! A dropped gate counts as settled: its worker has exited.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::time::Instant;

use crate::lifecycle::{CancellationToken, LifecycleError, LifecycleResult};

/// What the owner currently asks of its workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseState {
    Running,
    Paused,
    /// The activation is stopping; workers should exit.
    Closed,
}

#[derive(Debug, Clone, Copy)]
struct Request {
    epoch: u64,
    state: PauseState,
}

/// The owner side: requests Pause, Resume and close, and waits for workers.
pub struct PauseControl {
    request: watch::Sender<Request>,
    requested: watch::Sender<bool>,
    gates: Arc<Mutex<Vec<watch::Receiver<Option<u64>>>>>,
}

impl Default for PauseControl {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PauseControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseControl")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl PauseControl {
    pub fn new() -> Self {
        Self {
            request: watch::channel(Request {
                epoch: 0,
                state: PauseState::Running,
            })
            .0,
            requested: watch::channel(false).0,
            gates: Arc::default(),
        }
    }

    /// Registers a worker that must acknowledge every Pause.
    ///
    /// A worker whose gate is created while paused must still reach a boundary
    /// before it does any work.
    pub fn gate(&self) -> PauseGate {
        let (acknowledged, receiver) = watch::channel(None);
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates.retain(|gate| gate.has_changed().is_ok());
        gates.push(receiver);
        PauseGate {
            request: self.request.subscribe(),
            acknowledged,
        }
    }

    /// Observes whether Pause is requested, without acknowledging it.
    /// Admission paths use it to refuse new work from the moment of the request.
    pub fn requests(&self) -> watch::Receiver<bool> {
        self.requested.subscribe()
    }

    pub fn state(&self) -> PauseState {
        self.request.borrow().state
    }

    pub fn is_pause_requested(&self) -> bool {
        self.state() == PauseState::Paused
    }

    /// Whether every live gate has acknowledged the current Pause.
    pub fn is_paused(&self) -> bool {
        let request = *self.request.borrow();
        request.state == PauseState::Paused && self.pending(request.epoch).is_empty()
    }

    /// Requests Pause without waiting. Returns the request's epoch.
    pub fn request_pause(&self) -> u64 {
        let mut epoch = 0;
        self.request.send_modify(|request| {
            if request.state == PauseState::Running {
                request.epoch += 1;
                request.state = PauseState::Paused;
            }
            epoch = request.epoch;
        });
        self.requested.send_replace(self.is_pause_requested());
        epoch
    }

    /// Requests Pause and waits until every live gate acknowledges it.
    ///
    /// On cancellation (`Cancelled`) or expiry (`Retryable`) the request is
    /// withdrawn, so an incomplete Pause leaves the workers running as before.
    /// Pausing an already paused or closed control waits for the current state.
    pub async fn pause(
        &self,
        deadline: Option<Instant>,
        cancellation: &CancellationToken,
    ) -> LifecycleResult {
        let epoch = self.request_pause();
        if self.state() == PauseState::Closed {
            return Err(LifecycleError::terminal(anyhow::anyhow!(
                "cannot pause a stopping activation"
            )));
        }
        let settled = async {
            for mut gate in self.pending(epoch) {
                let _ = gate
                    .wait_for(|acknowledged| *acknowledged == Some(epoch))
                    .await;
            }
        };
        let deadline = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let outcome = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(LifecycleError::cancelled()),
            _ = settled => Ok(()),
            _ = deadline => Err(LifecycleError::retryable(anyhow::anyhow!(
                "{} worker(s) did not reach a pause boundary before the deadline",
                self.pending(epoch).len()
            ))),
        };
        if outcome.is_err() {
            self.resume();
        }
        outcome
    }

    /// Withdraws Pause. Has no effect once closed.
    pub fn resume(&self) {
        self.request.send_if_modified(|request| {
            let paused = request.state == PauseState::Paused;
            if paused {
                request.state = PauseState::Running;
            }
            paused
        });
        self.requested.send_replace(self.is_pause_requested());
    }

    /// Wakes every gate for Stop; [`PauseGate::wait_running`] then returns `false`.
    pub fn close(&self) {
        self.request
            .send_modify(|request| request.state = PauseState::Closed);
        self.requested.send_replace(false);
    }

    /// Reopens a closed control for a new activation.
    pub fn reopen(&self) {
        self.request.send_if_modified(|request| {
            let closed = request.state == PauseState::Closed;
            if closed {
                request.state = PauseState::Running;
            }
            closed
        });
    }

    fn pending(&self, epoch: u64) -> Vec<watch::Receiver<Option<u64>>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates.retain(|gate| gate.has_changed().is_ok());
        gates
            .iter()
            .filter(|gate| *gate.borrow() != Some(epoch))
            .cloned()
            .collect()
    }
}

/// The worker side of a [`PauseControl`]. Not `Clone`: each worker that must
/// stop emitting while paused takes its own gate.
pub struct PauseGate {
    request: watch::Receiver<Request>,
    acknowledged: watch::Sender<Option<u64>>,
}

impl std::fmt::Debug for PauseGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseGate")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl PauseGate {
    pub fn state(&self) -> PauseState {
        self.request.borrow().state
    }

    pub fn is_pause_requested(&self) -> bool {
        self.state() == PauseState::Paused
    }

    /// Call at a boundary where the worker holds no open work. Acknowledges a
    /// pending Pause, or clears the acknowledgement once running, and returns
    /// the state the worker must honour until its next boundary.
    pub fn sync(&mut self) -> PauseState {
        let request = *self.request.borrow_and_update();
        let acknowledged = (request.state == PauseState::Paused).then_some(request.epoch);
        self.acknowledged.send_if_modified(|current| {
            let changed = *current != acknowledged;
            *current = acknowledged;
            changed
        });
        request.state
    }

    /// Resolves when the requested state changes; cancellation safe. Never
    /// resolves once the control is dropped, so it is safe in a `select!` loop.
    pub async fn changed(&mut self) -> PauseState {
        match self.request.changed().await {
            Ok(()) => self.request.borrow().state,
            Err(_) => std::future::pending().await,
        }
    }

    /// Resolves once Pause is requested or the control closes; cancellation
    /// safe. Use it in `select!` around long waits, so a blocked worker can
    /// return to its boundary. Never resolves once the control is dropped.
    pub async fn pause_requested(&mut self) {
        if self
            .request
            .wait_for(|request| request.state != PauseState::Running)
            .await
            .is_err()
        {
            std::future::pending::<()>().await;
        }
    }

    /// At a boundary: acknowledges any Pause and waits until running.
    /// Returns `false` once the control closes or is dropped, when the worker
    /// should exit.
    pub async fn wait_running(&mut self) -> bool {
        loop {
            match self.sync() {
                PauseState::Running => return true,
                PauseState::Closed => return false,
                PauseState::Paused => {
                    if self.request.changed().await.is_err() {
                        return false;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn never() -> CancellationToken {
        CancellationToken::new()
    }

    #[tokio::test]
    async fn pause_waits_for_every_gate_and_resume_releases_them() {
        let control = PauseControl::new();
        let mut first = control.gate();
        let mut second = control.gate();
        let token = never();
        let pausing = control.pause(None, &token);
        tokio::pin!(pausing);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pausing)
                .await
                .is_err()
        );
        assert!(control.is_pause_requested() && !control.is_paused());
        assert_eq!(first.sync(), PauseState::Paused);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pausing)
                .await
                .is_err()
        );
        assert_eq!(second.sync(), PauseState::Paused);
        pausing.await.unwrap();
        assert!(control.is_paused());

        let worker = tokio::spawn(async move { first.wait_running().await });
        control.resume();
        assert!(worker.await.unwrap());
        assert_eq!(second.sync(), PauseState::Running);
    }

    #[tokio::test]
    async fn a_stale_acknowledgement_never_satisfies_a_later_pause() {
        let control = PauseControl::new();
        let mut gate = control.gate();
        control.request_pause();
        gate.sync();
        control.resume();
        // The worker has not yet observed Resume, so it still acknowledges the
        // first epoch; the second Pause must wait for a fresh acknowledgement.
        let token = never();
        let pausing = control.pause(None, &token);
        tokio::pin!(pausing);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut pausing)
                .await
                .is_err()
        );
        gate.sync();
        pausing.await.unwrap();
    }

    #[tokio::test]
    async fn expiry_and_cancellation_withdraw_the_pause() {
        let control = PauseControl::new();
        let _gate = control.gate();
        let mut requests = control.requests();
        let expired = control
            .pause(Some(Instant::now() + Duration::from_millis(10)), &never())
            .await
            .unwrap_err();
        assert!(expired.is_retryable());
        assert_eq!(control.state(), PauseState::Running);
        assert!(!*requests.borrow_and_update());

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled = control.pause(None, &cancellation).await.unwrap_err();
        assert_eq!(
            cancelled.kind(),
            crate::lifecycle::LifecycleErrorKind::Cancelled
        );
        assert_eq!(control.state(), PauseState::Running);
    }

    #[tokio::test]
    async fn dropped_gates_are_settled_and_close_wakes_waiters() {
        let control = PauseControl::new();
        drop(control.gate());
        let mut gate = control.gate();
        control.request_pause();
        let waiter = tokio::spawn(async move { gate.wait_running().await });
        tokio::task::yield_now().await;
        assert!(control.is_paused() || !waiter.is_finished());
        control.close();
        assert!(!waiter.await.unwrap());
        assert!(control.pause(None, &never()).await.is_err());
        control.reopen();
        assert_eq!(control.state(), PauseState::Running);
    }

    #[tokio::test]
    async fn pause_requested_interrupts_long_waits() {
        let control = PauseControl::new();
        let mut gate = control.gate();
        let worker = tokio::spawn(async move {
            tokio::select! {
                _ = gate.pause_requested() => {}
                _ = std::future::pending::<()>() => unreachable!(),
            }
            gate.sync()
        });
        tokio::task::yield_now().await;
        control.pause(None, &never()).await.unwrap();
        assert_eq!(worker.await.unwrap(), PauseState::Paused);
    }
}
