// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{ComponentObservations, GraphTelemetry, TelemetryError};
use metrics::{Counter, Gauge, Histogram};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum QueryPhase {
    Steady = 0,
    Bootstrap = 1,
}

impl QueryPhase {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Bootstrap => "bootstrap",
        }
    }
}

#[derive(Debug)]
pub(crate) struct QueryMetrics {
    pub inputs: [Counter; 2],
    pub results: [Counter; 3],
    pub engine: [Histogram; 2],
    pub commit: [[Histogram; 3]; 2],
    gauges: Mutex<Option<[Gauge; 2]>>,
}

impl QueryMetrics {
    pub(crate) fn new(config: &GraphTelemetry) -> Result<Self, TelemetryError> {
        let query = config
            .scope()
            .and_then(super::TelemetryScope::component_id)
            .expect("query component")
            .to_string();
        let labels = || vec![("query_id".into(), query.clone())];
        let phase_labels = |phase: QueryPhase| {
            let mut labels = labels();
            labels.push(("phase".into(), phase.label().into()));
            labels
        };
        let phases = [QueryPhase::Steady, QueryPhase::Bootstrap];
        let mut inputs = [Counter::noop(), Counter::noop()];
        let mut engine = [Histogram::noop(), Histogram::noop()];
        let mut commit = std::array::from_fn(|_| std::array::from_fn(|_| Histogram::noop()));
        for phase in phases {
            let index = phase as usize;
            inputs[index] = config
                .register("drasi.query.events_processed", phase_labels(phase))?
                .counter()?;
            engine[index] = config
                .register("drasi.query.engine.duration", phase_labels(phase))?
                .histogram()?;
            for (handle, outcome) in commit[index]
                .iter_mut()
                .zip(["success", "error", "cancelled"])
            {
                let mut labels = phase_labels(phase);
                labels.push(("outcome".into(), outcome.into()));
                *handle = config
                    .register("drasi.query.commit.duration", labels)?
                    .histogram()?;
            }
        }
        let mut results = [Counter::noop(), Counter::noop(), Counter::noop()];
        for (handle, kind) in results.iter_mut().zip(["add", "update", "delete"]) {
            let mut labels = labels();
            labels.push(("change_kind".into(), kind.into()));
            *handle = config
                .register("drasi.query.results_emitted", labels)?
                .counter()?;
        }
        Ok(Self {
            inputs,
            results,
            engine,
            commit,
            gauges: Mutex::new(None),
        })
    }

    pub(crate) fn state(
        &self,
        config: &GraphTelemetry,
        outbox: usize,
        live: usize,
    ) -> Result<(), TelemetryError> {
        if !["drasi.query.outbox.entries", "drasi.query.live_results"]
            .iter()
            .any(|name| config.policy().allows_metric(name))
        {
            return Ok(());
        }
        let mut gauges = self.gauges.lock().map_err(|_| TelemetryError::Poisoned)?;
        if gauges.is_none() {
            let query = config
                .scope()
                .and_then(super::TelemetryScope::component_id)
                .expect("query component");
            let labels = || vec![("query_id".into(), query.to_string())];
            *gauges = Some([
                config
                    .register("drasi.query.outbox.entries", labels())?
                    .gauge()?,
                config
                    .register("drasi.query.live_results", labels())?
                    .gauge()?,
            ]);
        }
        let gauges = gauges.as_ref().expect("registered query gauges");
        gauges[0].set(outbox as f64);
        gauges[1].set(live as f64);
        Ok(())
    }

    pub(crate) fn retire(&self) {
        let gauges = self.gauges.lock().unwrap_or_else(|error| {
            log::error!("Retiring poisoned query metric gauges: {error}");
            error.into_inner()
        });
        if let Some(gauges) = &*gauges {
            for gauge in gauges {
                gauge.set(0.0);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QueryEpoch {
    pub generation: u64,
    pub config_hash: u64,
    pub incarnation: Option<uuid::Uuid>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct QueryHead {
    pub epoch: QueryEpoch,
    pub sequence: u64,
}

#[derive(Debug, Default)]
struct ProgressState {
    head: Option<QueryHead>,
    subscribers: Vec<Weak<ReactionSubscriptionMetrics>>,
}

/// Transient, subscription-bounded observations. No storage or processing owner.
#[derive(Debug, Default)]
pub(crate) struct QueryProgressObservations(Mutex<ProgressState>);

impl QueryProgressObservations {
    pub(crate) fn bind(
        &self,
        subscription: &Arc<ReactionSubscriptionMetrics>,
        epoch: QueryEpoch,
    ) -> Result<(), TelemetryError> {
        if !subscription.observes_progress() {
            return Ok(());
        }
        let mut state = self.0.lock().map_err(|_| TelemetryError::Poisoned)?;
        state
            .subscribers
            .retain(|subscriber| subscriber.strong_count() > 0);
        if !state
            .subscribers
            .iter()
            .any(|existing| existing.ptr_eq(&Arc::downgrade(subscription)))
        {
            state.subscribers.push(Arc::downgrade(subscription));
        }
        subscription.bind_epoch(epoch)?;
        subscription.head(state.head)
    }

    pub(crate) fn publish(&self, head: Option<QueryHead>) {
        let mut state = self.0.lock().unwrap_or_else(|error| {
            log::error!("Publishing poisoned query metric progress: {error}");
            error.into_inner()
        });
        state.head = head;
        state.subscribers.retain(|subscriber| {
            let Some(subscriber) = subscriber.upgrade() else {
                return false;
            };
            if let Err(error) = subscriber.head(head) {
                log::warn!("Cannot observe query checkpoint lag: {error}");
            }
            true
        });
    }
}

#[derive(Debug, Default)]
struct CheckpointState {
    epoch: Option<QueryEpoch>,
    head: Option<QueryHead>,
    checkpoint: Option<(u64, u64)>,
    lag: Option<Gauge>,
}

#[derive(Debug)]
pub(crate) struct ReactionSubscriptionMetrics {
    owner: Weak<ComponentObservations>,
    config: GraphTelemetry,
    labels: Vec<(String, String)>,
    rows: Counter,
    dedup: Counter,
    gaps: Counter,
    recovery: [Counter; 3],
    checkpoint: Mutex<CheckpointState>,
}

impl ReactionSubscriptionMetrics {
    fn observes_progress(&self) -> bool {
        self.config
            .policy()
            .allows_metric("drasi.reaction.checkpoint_lag_events")
    }

    pub(crate) fn new(
        owner: &Arc<ComponentObservations>,
        config: &GraphTelemetry,
        query: &str,
    ) -> Result<Arc<Self>, TelemetryError> {
        let reaction = config
            .scope()
            .and_then(super::TelemetryScope::component_id)
            .ok_or_else(|| {
                TelemetryError::Invalid("reaction metrics require a component".into())
            })?;
        let labels =
            vec![("reaction_id".into(), reaction.to_string()), ("query_id".into(), query.into())];
        let mut recovery = [Counter::noop(), Counter::noop(), Counter::noop()];
        for (handle, policy) in recovery
            .iter_mut()
            .zip(["strict", "auto_reset", "auto_skip_gap"])
        {
            let mut labels = labels.clone();
            labels.push(("policy".into(), policy.into()));
            *handle = config
                .register("drasi.reaction.recovery_triggers", labels)?
                .counter()?;
        }
        Ok(Arc::new(Self {
            owner: Arc::downgrade(owner),
            config: config.clone(),
            rows: config
                .register("drasi.reaction.results_processed", labels.clone())?
                .counter()?,
            dedup: config
                .register("drasi.reaction.dedup_skips", labels.clone())?
                .counter()?,
            gaps: config
                .register("drasi.reaction.gaps_detected", labels.clone())?
                .counter()?,
            labels,
            recovery,
            checkpoint: Mutex::new(CheckpointState::default()),
        }))
    }

    pub(crate) fn handled(&self, rows: usize, inputs: usize) {
        if let Some(owner) = self.owner.upgrade() {
            owner.update(|| {
                self.rows.increment(rows as u64);
                owner.handled_inputs(inputs);
            });
        }
    }

    pub(crate) fn dedup(&self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.update(|| self.dedup.increment(1));
        }
    }

    pub(crate) fn gap(&self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.update(|| self.gaps.increment(1));
        }
    }

    pub(crate) fn recovery(&self, policy: crate::recovery::ReactionRecoveryPolicy) {
        let index = match policy {
            crate::recovery::ReactionRecoveryPolicy::Strict => 0,
            crate::recovery::ReactionRecoveryPolicy::AutoReset => 1,
            crate::recovery::ReactionRecoveryPolicy::AutoSkipGap => 2,
        };
        if let Some(owner) = self.owner.upgrade() {
            owner.update(|| self.recovery[index].increment(1));
        }
    }

    fn bind_epoch(&self, epoch: QueryEpoch) -> Result<(), TelemetryError> {
        let mut state = self
            .checkpoint
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        if state.epoch != Some(epoch) {
            state.epoch = Some(epoch);
            state.checkpoint = None;
        }
        Ok(())
    }

    fn head(&self, head: Option<QueryHead>) -> Result<(), TelemetryError> {
        let mut state = self
            .checkpoint
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        state.head = head;
        self.publish_lag(&mut state)
    }

    pub(crate) fn checkpoint(&self, sequence: u64, config_hash: u64) -> Result<(), TelemetryError> {
        let mut state = self
            .checkpoint
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        state.checkpoint = Some((sequence, config_hash));
        self.publish_lag(&mut state)
    }

    fn publish_lag(&self, state: &mut CheckpointState) -> Result<(), TelemetryError> {
        if !self.observes_progress() {
            return Ok(());
        }
        let Some((head, (sequence, hash), epoch)) = state
            .head
            .zip(state.checkpoint)
            .zip(state.epoch)
            .map(|((head, checkpoint), epoch)| (head, checkpoint, epoch))
        else {
            return Ok(());
        };
        if head.epoch != epoch || hash != epoch.config_hash || sequence > head.sequence {
            return Ok(());
        }
        if !self
            .config
            .policy()
            .allows_metric("drasi.reaction.checkpoint_lag_events")
        {
            return Ok(());
        }
        let Some(owner) = self.owner.upgrade() else {
            return Ok(());
        };
        let mut result = Ok(());
        owner.update(|| {
            if state.lag.is_none() {
                match self
                    .config
                    .register("drasi.reaction.checkpoint_lag_events", self.labels.clone())
                    .and_then(|metric| metric.gauge())
                {
                    Ok(gauge) => state.lag = Some(gauge),
                    Err(error) => result = Err(error),
                }
            }
            if let Some(lag) = &state.lag {
                lag.set((head.sequence - sequence) as f64);
            }
        });
        result
    }
}

pub(crate) type ReactionSubscriptions = BTreeMap<String, Arc<ReactionSubscriptionMetrics>>;
