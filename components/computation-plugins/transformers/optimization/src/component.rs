// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The optimization transformer: input dataset, solve scheduling, lifecycle
//! and management.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentDescriptor, ComponentUpdateData, ElementImage, ElementPatch,
    GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress, InputEnvelope, OutputEnvelope,
    OutputReaders, PortId, WakeupSource,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleError,
        LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
    },
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeTransformer, Scope, UpdateRequest,
};
use drasi_core_types::models::{Element, ElementReference, SourceChange};
use std::{
    collections::{HashMap, VecDeque},
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

use crate::{
    config::OptimizationConfig,
    model::Record,
    output::{self, EmittedRows, OutRow, Output, Rows},
    worker::{self, Events, Job, JobEvent, Wake},
};

/// Classifies a configuration change: a new output stream is a new producer
/// (Replace); everything else applies in place at a Pause and re-solves.
pub(crate) fn plan(current: &OptimizationConfig, proposed: &OptimizationConfig) -> Assessment {
    let boundary = if current.stream != proposed.stream {
        UpdateBoundary::Replace
    } else {
        UpdateBoundary::Pause
    };
    Assessment::supported(boundary, ComponentUpdateData::Preserve)
}

/// Pure planning used by the factory and the live instance.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure!(
        request.current.id == request.proposed.id
            && request.current.implementation == request.proposed.implementation
            && request.current.configuration_version == request.proposed.configuration_version,
        "updates cannot change component identity, implementation or configuration version"
    );
    let current = OptimizationConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = OptimizationConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    Ok(plan(&current, &proposed))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

fn record(element: &Element) -> Record {
    let metadata = element.get_metadata();
    Record {
        namespace: metadata.reference.namespace.clone(),
        id: metadata.reference.element_id.clone(),
        labels: metadata.labels.to_vec(),
        properties: element
            .get_properties()
            .map_iter(|name, value| (name.to_string(), serde_json::Value::from(value)))
            .collect(),
    }
}

pub(crate) struct Optimization {
    descriptor: ComponentDescriptor,
    config: Arc<OptimizationConfig>,
    identity: GraphProducerIdentity,
    output: Output,
    sequence: u64,
    started: bool,
    paused: bool,
    retired: bool,
    /// The current input dataset by element reference.
    inputs: HashMap<ElementReference, Element>,
    /// The latest input, from which emissions derive their lineage.
    last_input: Option<ChangeEnvelope>,
    /// The latest input effective time; emitted elements carry it.
    clock: u64,
    /// Input (or configuration) changed since the running or last job started.
    dirty: bool,
    first_dirty: Option<Instant>,
    last_change: Option<Instant>,
    /// Discards results of jobs started before a stop or update.
    generation: u64,
    job: Option<Job>,
    events: Events,
    wake: Arc<Wake>,
    /// Signature of the last fully solved model, skipped when unchanged.
    last_signature: Option<String>,
    /// Signature of the main result currently published.
    current_signature: Option<String>,
    current_provenance: serde_json::Value,
    solution: Rows,
    status: Option<(String, OutRow)>,
    scenarios: Rows,
    emitted: EmittedRows,
}

impl Optimization {
    pub(crate) fn new(
        descriptor: ComponentDescriptor,
        config: OptimizationConfig,
        scope: Option<&Scope>,
    ) -> Result<Self> {
        let identity = GraphProducerIdentity::volatile(
            scope
                .map_or("volatile", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .map_or("volatile", |scope| scope.graph_id.as_str())
                .into(),
            descriptor.id().clone(),
            config.stream.clone(),
        )?;
        Ok(Self {
            output: Output::new(identity.clone())?,
            descriptor,
            config: Arc::new(config),
            identity,
            sequence: 0,
            started: false,
            paused: false,
            retired: false,
            inputs: HashMap::new(),
            last_input: None,
            clock: 0,
            dirty: false,
            first_dirty: None,
            last_change: None,
            generation: 0,
            job: None,
            events: Arc::new(Mutex::new(VecDeque::new())),
            wake: Wake::new(),
            last_signature: None,
            current_signature: None,
            current_provenance: serde_json::Value::Null,
            solution: Rows::new(),
            status: None,
            scenarios: Rows::new(),
            emitted: EmittedRows::new(),
        })
    }

    fn namespace(&self) -> String {
        self.config
            .namespace
            .clone()
            .unwrap_or_else(|| self.descriptor.id().to_string())
    }

    fn mark_dirty(&mut self) {
        let now = Instant::now();
        self.dirty = true;
        self.first_dirty.get_or_insert(now);
        self.last_change = Some(now);
    }

    fn due(&self) -> Option<Instant> {
        if !self.dirty {
            return None;
        }
        let debounce = self
            .last_change
            .map(|at| at + Duration::from_millis(self.config.debounce_ms));
        let ceiling = self
            .first_dirty
            .map(|at| at + Duration::from_millis(self.config.max_delay_ms));
        match (debounce, ceiling) {
            (Some(debounce), Some(ceiling)) => Some(debounce.min(ceiling)),
            (debounce, ceiling) => debounce.or(ceiling),
        }
    }

    fn cancelling(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.cancel.load(Ordering::Acquire))
    }

    fn reschedule(&self) {
        let open = self.started && !self.paused && !self.retired;
        // A due solve waiting on a cancelled job is woken by the job's events.
        let deadline = self.due().filter(|_| !self.cancelling());
        self.wake.schedule(open, self.job.is_some(), deadline);
    }

    fn abandon_job(&mut self) {
        self.generation += 1;
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Release);
        }
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
        self.wake.clear_ready();
    }

    fn launch(&mut self) -> Result<()> {
        let mut records = self.inputs.values().map(record).collect::<Vec<_>>();
        records
            .sort_by(|left, right| (&left.namespace, &left.id).cmp(&(&right.namespace, &right.id)));
        self.job = Some(worker::spawn(
            self.config.clone(),
            records,
            self.last_signature.clone(),
            self.generation,
            self.events.clone(),
            self.wake.clone(),
        )?);
        self.dirty = false;
        self.first_dirty = None;
        self.last_change = None;
        Ok(())
    }

    fn accept(&mut self, event: JobEvent) {
        match event {
            JobEvent::Main(outcome) => {
                self.solution = output::solution_rows(&self.config, &outcome);
                self.status = Some(output::status_row(&self.config, &outcome));
                if outcome.signature.is_none() {
                    self.scenarios.clear();
                }
                self.current_signature = outcome
                    .signature
                    .clone()
                    .filter(|_| outcome.solved.status.is_final());
                self.current_provenance = outcome.provenance.clone();
            }
            JobEvent::Scenarios(outcomes) => {
                self.scenarios = output::scenario_rows(
                    &self.config,
                    &outcomes,
                    self.current_signature.as_deref(),
                    &self.current_provenance,
                );
                self.last_signature = self.current_signature.clone();
            }
            JobEvent::Unchanged => {}
            JobEvent::Finished => self.job = None,
        }
    }

    /// Emits the difference between the published rows and the current result.
    fn emit(&mut self) -> Result<Vec<OutputEnvelope>> {
        let Some(last_input) = &self.last_input else {
            return Ok(Vec::new());
        };
        let mut desired = self.solution.clone();
        desired.extend(self.scenarios.clone());
        if let Some((id, row)) = &self.status {
            desired.insert(id.clone(), row.clone());
        }
        let (next, changes) = output::diff(&self.emitted, &desired, &self.namespace(), self.clock);
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .context("optimization output sequence exhausted")?;
        let mut envelope = GraphChangeCodec::derive_changes(
            last_input,
            &changes,
            self.config.stream.clone(),
            sequence,
        )?;
        envelope.validate_lineage()?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        self.output.publish(&next, sequence)?;
        self.emitted = next;
        self.sequence = sequence;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }])
    }

    /// Applies one input batch to the dataset atomically.
    fn ingest(&mut self, changes: Vec<SourceChange>) -> Result<u64> {
        let mut staged: HashMap<ElementReference, Option<Element>> = HashMap::new();
        let mut clock = self.clock;
        for change in changes {
            change.validate()?;
            match change {
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    let metadata = element.get_metadata();
                    clock = clock.max(metadata.effective_from);
                    staged.insert(metadata.reference.clone(), Some(element));
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    let metadata = element.get_metadata();
                    clock = clock.max(metadata.effective_from);
                    let reference = metadata.reference.clone();
                    let base = match staged.get(&reference) {
                        Some(staged) => staged.as_ref(),
                        None => self.inputs.get(&reference),
                    }
                    .with_context(|| {
                        format!(
                            "optimization patch for {reference} has no retained full input image"
                        )
                    })?;
                    let patch = ElementPatch::try_new(
                        ElementImage::try_from(element)?,
                        removed_properties,
                    )?;
                    let patched = patch.apply_to(Some(base))?;
                    staged.insert(reference, Some(patched));
                }
                SourceChange::Delete { metadata } => {
                    clock = clock.max(metadata.effective_from);
                    staged.insert(metadata.reference, None);
                }
                SourceChange::Future { .. } => {
                    anyhow::bail!("optimization transforms do not accept scheduled notifications")
                }
            }
        }
        let mut count = self.inputs.len() as u64;
        for (reference, element) in &staged {
            match (self.inputs.contains_key(reference), element.is_some()) {
                (false, true) => count += 1,
                (true, false) => count -= 1,
                _ => {}
            }
        }
        ensure!(
            count <= self.config.max_records,
            "the input dataset would hold {count} records, more than maxRecords ({})",
            self.config.max_records
        );
        for (reference, element) in staged {
            match element {
                Some(element) => {
                    self.inputs.insert(reference, element);
                }
                None => {
                    self.inputs.remove(&reference);
                }
            }
        }
        Ok(clock)
    }

    #[cfg(test)]
    pub(crate) fn wake(&self) -> Arc<Wake> {
        self.wake.clone()
    }
}

#[async_trait]
impl NativeComponent for Optimization {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        self.config.normalized()
    }

    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_snapshot(
            PortId::try_new("out").expect("constant port"),
            self.output.reader(),
        )
    }

    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }

    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }

    /// Opens input and solving. A dataset retained from before a Stop is
    /// re-solved after the debounce.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.retired {
            return Err(anyhow::anyhow!("optimization state has been deprovisioned").into());
        }
        if self.started {
            return Err(anyhow::anyhow!("optimization already started").into());
        }
        self.output.set_running(true)?;
        self.started = true;
        self.paused = false;
        if self.last_input.is_some() {
            self.mark_dirty();
        }
        self.reschedule();
        Ok(())
    }

    /// Closes the wakeup gate: a running solve continues, and its results are
    /// published after Resume.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        if self.started {
            self.paused = true;
        }
        self.reschedule();
        Ok(())
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        self.reschedule();
        Ok(())
    }

    /// Discards in-flight and pending solves. Drain and Abort are the same:
    /// the published rows are already complete for the inputs they reflect.
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.abandon_job();
        self.dirty = false;
        self.first_dirty = None;
        self.last_change = None;
        if !self.retired {
            self.output.set_running(false)?;
        }
        self.started = false;
        self.paused = false;
        self.reschedule();
        Ok(())
    }

    /// Erase removes the dataset, the published result and the snapshot.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if self.started {
            return Err(anyhow::anyhow!("stop the optimization before removing its state").into());
        }
        if ctx.erases_owned_state() {
            self.abandon_job();
            self.output.retire()?;
            self.inputs.clear();
            self.last_input = None;
            self.last_signature = None;
            self.current_signature = None;
            self.current_provenance = serde_json::Value::Null;
            self.solution.clear();
            self.status = None;
            self.scenarios.clear();
            self.emitted.clear();
            self.retired = true;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for Optimization {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = OptimizationConfig::parse(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed = OptimizationConfig::parse(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != *self.config {
            return Err(refused(
                "the optimization configuration changed after the update was assessed",
            ));
        }
        let Some(plan) = plan(&current, &proposed).plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.started && !self.paused {
                    return Err(refused("in-place updates require a completed pause"));
                }
            }
            UpdateBoundary::Stop => {
                if self.started {
                    return Err(refused("restart updates require a stopped optimization"));
                }
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.config = Arc::new(proposed);
        self.abandon_job();
        self.last_signature = None;
        if self.last_input.is_some() {
            self.mark_dirty();
        }
        self.reschedule();
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for Optimization {
    /// Records the input; solving happens on a later wakeup, so nothing is
    /// emitted here.
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        ensure!(
            self.started && !self.retired,
            "optimization transformer is not running"
        );
        ensure!(
            input.port.as_str() == "in",
            "unknown optimization input port"
        );
        ensure!(
            input.envelope.system().control().is_none(),
            "optimization transforms accept graph data, not scheduling or recovery controls"
        );
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let changed = !changes.is_empty();
        self.clock = self.ingest(changes)?;
        self.last_input = Some(input.envelope);
        if changed {
            self.mark_dirty();
        }
        self.reschedule();
        Ok(Vec::new())
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.wake.clone())
    }

    async fn on_wakeup(&mut self) -> Result<Vec<OutputEnvelope>> {
        if !self.started || self.paused || self.retired {
            return Ok(Vec::new());
        }
        self.wake.clear_ready();
        let events = std::mem::take(
            &mut *self
                .events
                .lock()
                .map_err(|_| anyhow::anyhow!("optimization job queue poisoned"))?,
        );
        for (generation, event) in events {
            if generation == self.generation {
                self.accept(event);
            }
        }
        if self.due().is_some_and(|due| due <= Instant::now()) {
            match &self.job {
                None => self.launch()?,
                Some(job) => job.cancel.store(true, Ordering::Release),
            }
        }
        self.reschedule();
        self.emit()
    }
}

#[cfg(test)]
mod tests;
