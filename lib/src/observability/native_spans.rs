// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    decode_telemetry, CollectedSpan, CompletedSpan, CompletedSpanProcessor, PluginTelemetryConfig,
    RootSpanDecision, RootSpanRequest, TelemetryDiagnostics, TelemetryError, TelemetryLimits,
    TelemetryLoss, TelemetryLossSnapshot, TelemetrySubmission,
};
use drasi_computation_contracts::workers::{
    join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCleanupError, WorkerCompletion,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, RwLock};

#[derive(Debug, Clone, thiserror::Error)]
pub enum NativeTelemetryError {
    #[error(transparent)]
    Configuration(#[from] TelemetryError),
    #[error("native telemetry worker could not start: {0}")]
    Startup(Arc<std::io::Error>),
    #[error("native telemetry state poisoned")]
    Poisoned,
    #[error("native telemetry worker panicked")]
    WorkerPanic,
    #[error("native telemetry drain timed out after {0:?}; cleanup remains owned")]
    TimedOut(Duration),
    #[error("native telemetry cleanup cancelled; retry to join the retained worker")]
    Cancelled,
    #[error("native telemetry cleanup failed: {0}")]
    Cleanup(Arc<WorkerCleanupError>),
}

type Result<T> = std::result::Result<T, NativeTelemetryError>;

struct Queue {
    open: bool,
    records: usize,
    bytes: usize,
    pending: VecDeque<Pending>,
}

struct State {
    queue: Mutex<Queue>,
    changed: Condvar,
    drained: Notify,
    limits: TelemetryLimits,
    diagnostics: TelemetryDiagnostics,
}

struct Reservation {
    state: Arc<State>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        match self.state.queue.lock() {
            Ok(mut queue) => {
                queue.records -= 1;
                queue.bytes -= self.bytes;
                self.state.changed.notify_all();
                if queue.records == 0 {
                    self.state.drained.notify_waiters();
                }
            }
            Err(error) => log::error!("Releasing poisoned native telemetry reservation: {error}"),
        }
    }
}

struct Pending {
    span: CollectedSpan,
    _reservation: Reservation,
}

impl State {
    fn close(&self) {
        match self.queue.lock() {
            Ok(mut queue) => {
                queue.open = false;
                self.changed.notify_all();
            }
            Err(error) => log::error!("Closing poisoned native span collection: {error}"),
        }
    }

    fn discard(&self, reason: TelemetryLoss) -> Result<()> {
        let pending = {
            let mut queue = self
                .queue
                .lock()
                .map_err(|_| NativeTelemetryError::Poisoned)?;
            std::mem::take(&mut queue.pending)
        };
        self.diagnostics.record_many(reason, pending.len() as u64);
        drop(pending);
        Ok(())
    }
}

struct Owner {
    state: Arc<State>,
    processor: Arc<dyn CompletedSpanProcessor>,
    thread: Mutex<Option<std::thread::JoinHandle<Result<()>>>>,
    join: RwLock<Option<tokio::task::JoinHandle<Result<()>>>>,
    cleanup: tokio::sync::Mutex<()>,
    terminal: Mutex<Option<Result<()>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.state.close();
        if self.thread.get_mut().is_ok_and(|thread| thread.is_some()) {
            log::warn!("Native span collector dropped without awaited shutdown; drain requested");
        }
    }
}

/// Application-owned collection shared across native bindings. Shutdown does
/// not shut down the supplied processor/exporter or stop component producers.
#[derive(Clone)]
pub struct NativeSpanCollector(Arc<Owner>);

impl std::fmt::Debug for NativeSpanCollector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeSpanCollector")
            .field("limits", &self.0.state.limits)
            .field("losses", &self.losses())
            .finish_non_exhaustive()
    }
}

impl NativeSpanCollector {
    pub fn new(
        processor: Arc<dyn CompletedSpanProcessor>,
        limits: TelemetryLimits,
    ) -> Result<Self> {
        let limits = limits.validate()?;
        let state = Arc::new(State {
            queue: Mutex::new(Queue {
                open: true,
                records: 0,
                bytes: 0,
                pending: VecDeque::new(),
            }),
            changed: Condvar::new(),
            drained: Notify::new(),
            limits,
            diagnostics: Default::default(),
        });
        let worker_state = state.clone();
        let worker_processor = processor.clone();
        let thread = std::thread::Builder::new()
            .name("drasi-native-telemetry".into())
            .spawn(move || run(worker_state, worker_processor))
            .map_err(|error| NativeTelemetryError::Startup(Arc::new(error)))?;
        Ok(Self(Arc::new(Owner {
            state,
            processor,
            thread: Mutex::new(Some(thread)),
            join: RwLock::new(None),
            cleanup: tokio::sync::Mutex::new(()),
            terminal: Mutex::new(None),
        })))
    }

    pub fn limits(&self) -> TelemetryLimits {
        self.0.state.limits
    }
    pub fn losses(&self) -> TelemetryLossSnapshot {
        self.0.state.diagnostics.snapshot()
    }
    pub fn retained(&self) -> Result<(usize, usize)> {
        let queue = self
            .0
            .state
            .queue
            .lock()
            .map_err(|_| NativeTelemetryError::Poisoned)?;
        Ok((queue.records, queue.bytes))
    }

    pub fn sample_root(
        &self,
        config: &PluginTelemetryConfig,
        request: &RootSpanRequest,
    ) -> super::Result<RootSpanDecision> {
        self.0.processor.sample_root(config, request)
    }

    pub fn submit(&self, config: &PluginTelemetryConfig, bytes: &[u8]) -> TelemetrySubmission {
        let state = &self.0.state;
        if bytes.len() > state.limits.record_bytes || bytes.len() > config.limits.record_bytes {
            state.diagnostics.record(TelemetryLoss::Invalid);
            return TelemetrySubmission::Invalid;
        }
        let reservation = {
            let mut queue = match state.queue.lock() {
                Ok(queue) => queue,
                Err(error) => {
                    log::error!("Native span admission poisoned: {error}");
                    state.diagnostics.record(TelemetryLoss::Invalid);
                    return TelemetrySubmission::Invalid;
                }
            };
            if !queue.open {
                drop(queue);
                state.diagnostics.record(TelemetryLoss::Closed);
                return TelemetrySubmission::Closed;
            }
            if queue.records >= state.limits.records
                || bytes.len() > state.limits.bytes.saturating_sub(queue.bytes)
            {
                drop(queue);
                state.diagnostics.record(TelemetryLoss::Full);
                return TelemetrySubmission::Full;
            }
            queue.records += 1;
            queue.bytes += bytes.len();
            Reservation {
                state: state.clone(),
                bytes: bytes.len(),
            }
        };
        let span: CompletedSpan = match decode_telemetry(bytes, state.limits.record_bytes) {
            Ok(span) => span,
            Err(_) => {
                state.diagnostics.record(TelemetryLoss::Invalid);
                return TelemetrySubmission::Invalid;
            }
        };
        let pending = Pending {
            span: CollectedSpan {
                scope: config.scope.clone(),
                generation: config.generation,
                plugin_kind: config.plugin_kind.clone(),
                span,
            },
            _reservation: reservation,
        };
        let mut queue = match state.queue.lock() {
            Ok(queue) => queue,
            Err(error) => {
                log::error!("Native span publication poisoned: {error}");
                state.diagnostics.record(TelemetryLoss::Invalid);
                return TelemetrySubmission::Invalid;
            }
        };
        if !queue.open {
            drop(queue);
            state.diagnostics.record(TelemetryLoss::Closed);
            return TelemetrySubmission::Closed;
        }
        queue.pending.push_back(pending);
        state.changed.notify_one();
        TelemetrySubmission::Accepted
    }

    pub async fn flush(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.0.state.drained.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.retained()?.0 == 0 {
                    return Ok(());
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| NativeTelemetryError::TimedOut(timeout))?
    }

    pub async fn shutdown(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| TelemetryError::Invalid("invalid telemetry shutdown deadline".into()))?;
        self.0.state.close();
        let _cleanup = tokio::time::timeout_at(deadline, self.0.cleanup.lock())
            .await
            .map_err(|_| NativeTelemetryError::TimedOut(timeout))?;
        if let Some(result) = self
            .0
            .terminal
            .lock()
            .map_err(|_| NativeTelemetryError::Poisoned)?
            .clone()
        {
            return result;
        }
        if self.0.join.read().await.is_none() {
            let owner = self.0.clone();
            spawn_owned_blocking_worker(&self.0.join, move || {
                let thread = owner
                    .thread
                    .lock()
                    .map_err(|_| NativeTelemetryError::Poisoned)?
                    .take()
                    .ok_or_else(|| {
                        TelemetryError::Invalid("native telemetry join owner is missing".into())
                    })?;
                thread
                    .join()
                    .map_err(|_| NativeTelemetryError::WorkerPanic)?
            })
            .await
            .map_err(|error| {
                TelemetryError::Invalid(format!(
                    "native telemetry cleanup is already owned: {error}"
                ))
            })?;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let result = join_owned_worker_gracefully(&mut *self.0.join.write().await, remaining).await;
        let result = match result {
            Ok(WorkerCompletion::Completed(result)) => result,
            Ok(WorkerCompletion::Cancelled) => return Err(NativeTelemetryError::Cancelled),
            Ok(WorkerCompletion::Absent) => {
                Err(TelemetryError::Invalid("native telemetry join owner is missing".into()).into())
            }
            Err(WorkerCleanupError::TimedOut { .. }) => {
                self.0.state.discard(TelemetryLoss::ShutdownTimeout)?;
                return Err(NativeTelemetryError::TimedOut(timeout));
            }
            Err(error) => Err(NativeTelemetryError::Cleanup(Arc::new(error))),
        };
        *self
            .0
            .terminal
            .lock()
            .map_err(|_| NativeTelemetryError::Poisoned)? = Some(result.clone());
        result
    }
}

fn run(state: Arc<State>, processor: Arc<dyn CompletedSpanProcessor>) -> Result<()> {
    struct Exit {
        state: Arc<State>,
        complete: bool,
    }
    impl Drop for Exit {
        fn drop(&mut self) {
            if !self.complete {
                self.state.close();
                if let Err(error) = self.state.discard(TelemetryLoss::Exporter) {
                    log::error!("Discarding failed native telemetry work: {error}");
                }
            }
        }
    }
    let mut exit = Exit {
        state: state.clone(),
        complete: false,
    };
    loop {
        let pending = {
            let mut queue = state
                .queue
                .lock()
                .map_err(|_| NativeTelemetryError::Poisoned)?;
            loop {
                if let Some(pending) = queue.pending.pop_front() {
                    break Some(pending);
                }
                if !queue.open && queue.records == 0 {
                    break None;
                }
                queue = state
                    .changed
                    .wait(queue)
                    .map_err(|_| NativeTelemetryError::Poisoned)?;
            }
        };
        let Some(pending) = pending else { break };
        struct Processing<'a> {
            diagnostics: &'a TelemetryDiagnostics,
            complete: bool,
        }
        impl Drop for Processing<'_> {
            fn drop(&mut self) {
                if !self.complete {
                    self.diagnostics.record(TelemetryLoss::Exporter);
                }
            }
        }
        let mut processing = Processing {
            diagnostics: &state.diagnostics,
            complete: false,
        };
        if let Err(error) = processor.process(pending.span) {
            log::debug!("Native span processor rejected a record: {error}");
        } else {
            processing.complete = true;
        }
        drop(processing);
        drop(pending._reservation);
    }
    exit.complete = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{
        encode_telemetry, PluginRootSampling, TelemetryAttribute, TelemetryScope,
        TelemetrySpanStatus, TraceContextV1,
    };
    use drasi_computation_contracts::{ComponentRole, ComponentSemanticKind};

    #[derive(Debug)]
    struct Processor {
        gate: (Mutex<bool>, Condvar),
        started: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
        spans: Mutex<Vec<CollectedSpan>>,
    }
    impl Processor {
        fn release(&self) {
            *self.gate.0.lock().unwrap() = true;
            self.gate.1.notify_all();
        }
    }
    impl CompletedSpanProcessor for Processor {
        fn sample_root(
            &self,
            _: &PluginTelemetryConfig,
            _: &RootSpanRequest,
        ) -> super::super::Result<RootSpanDecision> {
            Ok(RootSpanDecision {
                sampled: true,
                trace_state: None,
            })
        }
        fn process(&self, span: CollectedSpan) -> super::super::Result<()> {
            if let Some(started) = self.started.lock().unwrap().take() {
                started.send(()).unwrap();
            }
            let mut gate = self.gate.0.lock().unwrap();
            while !*gate {
                gate = self.gate.1.wait(gate).unwrap();
            }
            self.spans.lock().unwrap().push(span);
            Ok(())
        }
    }
    struct Release(Arc<Processor>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn setup(
        limits: TelemetryLimits,
    ) -> (
        NativeSpanCollector,
        Arc<Processor>,
        Release,
        std::sync::mpsc::Receiver<()>,
        PluginTelemetryConfig,
        Vec<u8>,
    ) {
        let (started, receiver) = std::sync::mpsc::sync_channel(1);
        let processor = Arc::new(Processor {
            gate: (Mutex::new(false), Condvar::new()),
            started: Mutex::new(Some(started)),
            spans: Mutex::new(vec![]),
        });
        let collector = NativeSpanCollector::new(processor.clone(), limits).unwrap();
        let config = PluginTelemetryConfig {
            scope: TelemetryScope::graph("instance", "graph")
                .unwrap()
                .component(
                    crate::test_helpers::component_id("component"),
                    ComponentRole::Service,
                    ComponentSemanticKind::Service,
                ),
            generation: 1,
            plugin_kind: "test".into(),
            tracing: true,
            metrics: false,
            native_metrics_required: false,
            trace_directives: "trace".into(),
            log_directives: "off".into(),
            metric_exclusions: vec![],
            sampling: PluginRootSampling::Always,
            limits,
        };
        let record = CompletedSpan {
            name: "plugin.work".into(),
            target: "test".into(),
            context: TraceContextV1::new([1; 16], [2; 8], 1, None).unwrap(),
            parent_span_id: Some([3; 8]),
            links: vec![],
            start_time_unix_ns: 10,
            end_time_unix_ns: 20,
            status: TelemetrySpanStatus::Ok,
            attributes: vec![],
            events: vec![],
        };
        let bytes = encode_telemetry(&record, limits.record_bytes).unwrap();
        (
            collector,
            processor.clone(),
            Release(processor),
            receiver,
            config,
            bytes,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reservations_and_inflight_records_share_count_and_byte_budgets() {
        let limits = TelemetryLimits {
            records: 4,
            bytes: 8192,
            record_bytes: 4096,
            ..Default::default()
        };
        let (collector, processor, _release, started, config, bytes) = setup(limits);
        assert_eq!(
            collector.submit(&config, &bytes),
            TelemetrySubmission::Accepted
        );
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::scope(|threads| {
            for _ in 0..8 {
                let collector = collector.clone();
                let (config, bytes) = (&config, &bytes);
                threads.spawn(move || {
                    for _ in 0..4 {
                        collector.submit(config, bytes);
                    }
                });
            }
        });
        assert_eq!(collector.retained().unwrap(), (4, 4 * bytes.len()));
        assert_eq!(collector.losses().full, 29);
        assert_eq!(
            collector.submit(&config, &vec![0; 4097]),
            TelemetrySubmission::Invalid
        );
        assert_eq!(collector.losses().invalid, 1);
        processor.release();
        collector.flush(Duration::from_secs(2)).await.unwrap();
        assert_eq!(collector.retained().unwrap(), (0, 0));
        assert_eq!(processor.spans.lock().unwrap().len(), 4);
        collector.shutdown(Duration::from_secs(2)).await.unwrap();
        assert_eq!(
            collector.submit(&config, &bytes),
            TelemetrySubmission::Closed
        );
        assert_eq!(collector.losses().closed, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn byte_budget_and_timeout_discard_are_independent_of_count_capacity() {
        let limits = TelemetryLimits {
            records: 64,
            bytes: 8192,
            record_bytes: 4096,
            ..Default::default()
        };
        let (collector, processor, _release, started, config, bytes) = setup(limits);
        let mut record: CompletedSpan = decode_telemetry(&bytes, limits.record_bytes).unwrap();
        record.attributes.push((
            "padding".into(),
            TelemetryAttribute::String("x".repeat(2800)),
        ));
        let bytes = encode_telemetry(&record, limits.record_bytes).unwrap();
        let expected = limits.bytes / bytes.len();
        for _ in 0..expected {
            assert_eq!(
                collector.submit(&config, &bytes),
                TelemetrySubmission::Accepted
            );
        }
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(collector.submit(&config, &bytes), TelemetrySubmission::Full);
        assert_eq!(
            collector.retained().unwrap(),
            (expected, expected * bytes.len())
        );
        assert!(matches!(
            collector.shutdown(Duration::from_millis(10)).await,
            Err(NativeTelemetryError::TimedOut(_))
        ));
        assert_eq!(collector.losses().shutdown_timeout, (expected - 1) as u64);
        assert_eq!(collector.retained().unwrap(), (1, bytes.len()));
        processor.release();
        collector.shutdown(Duration::from_secs(2)).await.unwrap();
        assert_eq!(processor.spans.lock().unwrap().len(), 1);
        assert_eq!(collector.retained().unwrap(), (0, 0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_shutdown_retains_cleanup_and_invalid_records_release_reservations() {
        let limits = TelemetryLimits {
            records: 4,
            bytes: 8192,
            record_bytes: 4096,
            ..Default::default()
        };
        let (collector, processor, _release, started, config, bytes) = setup(limits);
        assert_eq!(
            collector.submit(&config, &[0xdd, 0xff, 0xff, 0xff, 0xff]),
            TelemetrySubmission::Invalid
        );
        assert_eq!(collector.retained().unwrap(), (0, 0));
        assert_eq!(
            collector.submit(&config, &bytes),
            TelemetrySubmission::Accepted
        );
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            collector.shutdown(Duration::from_secs(5))
        )
        .await
        .is_err());
        assert_eq!(collector.retained().unwrap().0, 1);
        processor.release();
        collector.shutdown(Duration::from_secs(2)).await.unwrap();
        assert_eq!(collector.losses().shutdown_timeout, 0);
    }
}
