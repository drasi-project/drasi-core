// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Background solving: one job thread at a time, results queued for the next
//! host wakeup, and the wakeup source the host waits on.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::WakeupSource;
use serde_json::Value;
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::{sync::watch, time::Instant};

use crate::{
    config::OptimizationConfig,
    model::{self, MainOutcome, Record, ScenarioOutcome, Solved, Status},
};

/// What a job reports, in order: `Main`, then `Scenarios` unless cancelled
/// (or `Unchanged` instead of both), and always `Finished` last.
#[derive(Debug)]
pub(crate) enum JobEvent {
    Main(Box<MainOutcome>),
    Scenarios(Vec<ScenarioOutcome>),
    /// The model signature equals the last completed solve; nothing to publish.
    Unchanged,
    Finished,
}

/// Runs one job synchronously, reporting each event as it completes.
pub(crate) fn run(
    config: &OptimizationConfig,
    records: &[Record],
    skip: Option<&str>,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(JobEvent),
) {
    let prepared = match model::prepare(config, records) {
        Ok(prepared) => prepared,
        Err(error) => {
            report(JobEvent::Main(Box::new(MainOutcome {
                signature: None,
                provenance: Value::Null,
                solved: Solved {
                    status: if error.downcast_ref::<model::LimitExceeded>().is_some() {
                        Status::Limit
                    } else {
                        Status::Error
                    },
                    detail: format!("{error:#}"),
                    objectives: Vec::new(),
                    values: Vec::new(),
                },
                model: None,
                variables: 0,
                constraints: 0,
                members: 0,
            })));
            return;
        }
    };
    if skip == Some(prepared.signature.as_str()) {
        report(JobEvent::Unchanged);
        return;
    }
    report(JobEvent::Main(Box::new(model::solve_main(
        config, &prepared,
    ))));
    let mut outcomes = Vec::new();
    let mut solved = 0_u64;
    for definition in &config.scenarios {
        // Scenario sets were validated against the configuration.
        let values = model::scenario_values(definition, &prepared).unwrap_or_default();
        for (key, value) in values {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let result = if solved < config.max_scenarios {
                solved += 1;
                model::solve_scenario(config, records, definition, &key)
            } else {
                Solved {
                    status: Status::Limit,
                    detail: format!(
                        "more than maxScenarios ({}) scenarios",
                        config.max_scenarios
                    ),
                    objectives: Vec::new(),
                    values: Vec::new(),
                }
            };
            outcomes.push(ScenarioOutcome {
                scenario: definition.name.clone(),
                property: definition.property.clone(),
                value,
                key,
                status: result.status,
                detail: result.detail,
                objectives: result.objectives,
            });
        }
    }
    report(JobEvent::Scenarios(outcomes));
}

#[derive(Clone, Copy, Debug, Default)]
struct WakeState {
    /// Started, not paused and not retired.
    open: bool,
    /// Job events are queued.
    ready: bool,
    /// A job is running.
    busy: bool,
    /// When pending input changes are due for a solve.
    deadline: Option<Instant>,
}

/// The host's wakeup source. `wait` returns only while open and when queued
/// events or a due deadline need `on_wakeup`; it never spins.
pub(crate) struct Wake(watch::Sender<WakeState>);

impl Wake {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(watch::Sender::new(WakeState::default())))
    }

    pub(crate) fn clear_ready(&self) {
        self.0
            .send_if_modified(|state| std::mem::take(&mut state.ready));
    }

    fn notify(&self) {
        self.0
            .send_if_modified(|state| !std::mem::replace(&mut state.ready, true));
    }

    pub(crate) fn schedule(&self, open: bool, busy: bool, deadline: Option<Instant>) {
        self.0.send_if_modified(|state| {
            let changed = state.open != open || state.busy != busy || state.deadline != deadline;
            state.open = open;
            state.busy = busy;
            state.deadline = deadline;
            changed
        });
    }

    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        let state = *self.0.borrow();
        state.open && state.ready
    }
}

#[async_trait]
impl WakeupSource for Wake {
    async fn wait(&self) -> Result<()> {
        let mut receiver = self.0.subscribe();
        loop {
            let state = *receiver.borrow_and_update();
            if state.open && state.ready {
                return Ok(());
            }
            match state.deadline.filter(|_| state.open) {
                Some(deadline) => tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return Ok(()),
                    changed = receiver.changed() => changed?,
                },
                None => receiver.changed().await?,
            }
        }
    }

    async fn has_pending(&self) -> Result<bool> {
        let state = *self.0.borrow();
        Ok(state.open && (state.ready || state.busy || state.deadline.is_some()))
    }
}

pub(crate) type Events = Arc<Mutex<VecDeque<(u64, JobEvent)>>>;

/// A running job. Cancellation is honoured between scenarios.
pub(crate) struct Job {
    pub cancel: Arc<AtomicBool>,
}

/// Starts a job thread for `generation`.
pub(crate) fn spawn(
    config: Arc<OptimizationConfig>,
    records: Vec<Record>,
    skip: Option<String>,
    generation: u64,
    events: Events,
    wake: Arc<Wake>,
) -> Result<Job> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    std::thread::Builder::new()
        .name("drasi-optimization".into())
        .spawn(move || {
            let push = |event: JobEvent| {
                if let Ok(mut queue) = events.lock() {
                    queue.push_back((generation, event));
                }
                wake.notify();
            };
            let mut report = |event: JobEvent| push(event);
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run(&config, &records, skip.as_deref(), &flag, &mut report)
            }));
            if outcome.is_err() {
                push(JobEvent::Main(Box::new(MainOutcome {
                    signature: None,
                    provenance: Value::Null,
                    solved: Solved {
                        status: Status::Error,
                        detail: "the solver panicked".into(),
                        objectives: Vec::new(),
                        values: Vec::new(),
                    },
                    model: None,
                    variables: 0,
                    constraints: 0,
                    members: 0,
                })));
            }
            push(JobEvent::Finished);
        })?;
    Ok(Job { cancel })
}
