// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The policy transformer: lifecycle, incremental evaluation, management and
//! continuation of re-evaluated decisions.

use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentDescriptor, ElementImage, ElementPatch, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, InputBinding, InputEnvelope, OutputEnvelope,
    OutputReaders, PortId, SystemMetadata,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleError,
        LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
    },
    update_planning::{ensure_same_component, inputs_changed},
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeTransformer, Scope,
    SnapshotSource, UpdateRequest, UpstreamSnapshots,
};
use drasi_core_types::models::{Element, ElementReference, SourceChange};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use crate::{
    config::PolicyConfig,
    decide::Live,
    output::{Output, Rows},
};

#[cfg(test)]
mod tests;

/// Most decisions one continuation envelope carries.
pub(crate) const CONTINUATION_BATCH: usize = 1024;
/// Re-evaluations between cooperative yields during an update.
const YIELD_EVERY: usize = 256;

type Key = (Arc<str>, Arc<str>);

fn key(reference: &ElementReference) -> Key {
    (reference.namespace.clone(), reference.element_id.clone())
}

/// Labels and properties: the observable content of a decision row.
fn same_row(left: &Element, right: &Element) -> bool {
    left.get_metadata().labels == right.get_metadata().labels
        && left.get_properties() == right.get_properties()
}

/// The change that moves the published row of `reference` to `desired`.
fn row_change(
    published: Option<&Element>,
    desired: Option<&Element>,
    effective_from: u64,
) -> Option<SourceChange> {
    match (published, desired) {
        (None, None) => None,
        (None, Some(desired)) => Some(SourceChange::Insert {
            element: desired.clone(),
        }),
        (Some(published), Some(desired)) if same_row(published, desired) => None,
        (Some(_), Some(desired)) => Some(SourceChange::Replace {
            element: desired.clone(),
        }),
        (Some(published), None) => {
            let mut metadata = published.get_metadata().clone();
            metadata.effective_from = effective_from;
            Some(SourceChange::Delete { metadata })
        }
    }
}

/// Pure planning used by the factory and the live instance. The proposed
/// configuration is validated, and its policy compiled, exactly as
/// construction would.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = PolicyConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = PolicyConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    Live::new(proposed.clone()).context("proposed configuration")?;
    Ok(current.plan(&proposed, inputs_changed(request)))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

pub(crate) struct PolicyTransformer {
    descriptor: ComponentDescriptor,
    live: Live,
    identity: GraphProducerIdentity,
    output: Output,
    sequence: u64,
    /// Complete images of every retained, label-matching input element.
    inputs: HashMap<ElementReference, Element>,
    /// The decision rows downstream has been sent.
    published: Rows,
    /// Re-evaluated decisions that differ from `published`, in key order.
    pending: BTreeMap<Key, (ElementReference, Option<Element>)>,
    /// Latest input effective time; re-evaluated decisions carry it.
    high_water: u64,
    last_system: Option<Arc<SystemMetadata>>,
    /// Upstream snapshot reads, when the host bound `output_snapshots`.
    snapshots: Option<Arc<dyn SnapshotSource>>,
    upstreams: UpstreamSnapshots,
    /// Whether the upstream snapshots have been loaded.
    loaded: bool,
    started: bool,
    paused: bool,
    retired: bool,
}

impl PolicyTransformer {
    pub(crate) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
        scope: Option<&Scope>,
        inputs: &[InputBinding],
        snapshots: Option<Arc<dyn SnapshotSource>>,
    ) -> Result<Self> {
        let live = Live::new(PolicyConfig::parse(configuration)?)?;
        let identity = GraphProducerIdentity::volatile(
            scope
                .map_or("volatile", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .map_or("volatile", |scope| scope.graph_id.as_str())
                .into(),
            descriptor.id().clone(),
            live.config.stream.clone(),
        )?;
        let output = Output::new(identity.clone())?;
        Ok(Self {
            descriptor,
            live,
            identity,
            output,
            sequence: 0,
            inputs: HashMap::new(),
            published: Rows::new(),
            pending: BTreeMap::new(),
            high_water: 0,
            last_system: None,
            upstreams: UpstreamSnapshots::new(inputs, &PortId::try_new("in")?),
            snapshots,
            loaded: false,
            started: false,
            paused: false,
            retired: false,
        })
    }

    fn ensure_running(&self) -> Result<()> {
        ensure!(
            self.started && !self.paused && !self.retired,
            "policy transformer is not running"
        );
        Ok(())
    }

    fn next_sequence(&self) -> Result<u64> {
        self.sequence
            .checked_add(1)
            .context("policy output sequence exhausted")
    }

    /// Annotates and publishes one output envelope, then commits its sequence.
    fn emit(
        &mut self,
        mut envelope: ChangeEnvelope,
        sequence: u64,
        rows: Vec<(ElementReference, Option<Element>)>,
    ) -> Result<OutputEnvelope> {
        envelope.validate_lineage()?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        self.output.publish(rows.iter().cloned(), sequence)?;
        for (reference, row) in rows {
            match row {
                Some(row) => self.published.insert(reference, row),
                None => self.published.remove(&reference),
            };
        }
        self.sequence = sequence;
        Ok(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        })
    }

    /// Applies one input batch to a staged copy of the retained inputs and
    /// returns the touched references in first-touch order with their latest
    /// effective time. Nothing is committed here.
    fn stage(
        &self,
        changes: Vec<SourceChange>,
    ) -> Result<(
        Vec<(ElementReference, u64)>,
        HashMap<ElementReference, Option<Element>>,
    )> {
        let mut staged: HashMap<ElementReference, Option<Element>> = HashMap::new();
        let mut touched: Vec<(ElementReference, u64)> = Vec::new();
        let mut positions: HashMap<ElementReference, usize> = HashMap::new();
        for change in changes {
            change.validate()?;
            let metadata = match &change {
                SourceChange::Insert { element }
                | SourceChange::Replace { element }
                | SourceChange::Update { element, .. } => element.get_metadata(),
                SourceChange::Delete { metadata } => metadata,
                SourceChange::Future { .. } => {
                    bail!("policy transforms do not accept scheduled notifications")
                }
            };
            let reference = metadata.reference.clone();
            match positions.get(&reference) {
                Some(&index) => touched[index].1 = metadata.effective_from,
                None => {
                    positions.insert(reference.clone(), touched.len());
                    touched.push((reference.clone(), metadata.effective_from));
                }
            }
            let next = match change {
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    Some(element).filter(|element| self.live.selects(element))
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    let base = match staged.get(&reference) {
                        Some(staged) => staged.as_ref(),
                        None => self.inputs.get(&reference),
                    };
                    match base {
                        Some(base) => {
                            let patch = ElementPatch::try_new(
                                ElementImage::try_from(element)?,
                                removed_properties,
                            )?;
                            Some(patch.apply_to(Some(base))?)
                                .filter(|element| self.live.selects(element))
                        }
                        // A patch of an element outside the label filter is ignored.
                        None if !self.live.selects(&element) => None,
                        None => {
                            bail!("policy input patch for {reference} has no retained full image")
                        }
                    }
                }
                SourceChange::Delete { .. } => None,
                SourceChange::Future { .. } => unreachable!("rejected above"),
            };
            staged.insert(reference, next);
        }
        let mut retained = self.inputs.len();
        for (reference, element) in &staged {
            match (self.inputs.contains_key(reference), element.is_some()) {
                (false, true) => retained += 1,
                (true, false) => retained -= 1,
                _ => {}
            }
        }
        ensure!(
            retained as u64 <= self.live.config.max_retained_inputs,
            "policy transformer would retain {retained} inputs, above max_retained_inputs {}",
            self.live.config.max_retained_inputs
        );
        Ok((touched, staged))
    }

    /// Adopts the upstream snapshots' elements and queues every decision that
    /// differs from the published one as a continuation.
    fn initialize_from(&mut self, changes: Vec<SourceChange>) -> Result<()> {
        let (touched, mut staged) = self.stage(changes)?;
        let mut decided = Vec::with_capacity(touched.len());
        for (reference, effective_from) in &touched {
            let desired = match staged.get(reference).and_then(Option::as_ref) {
                Some(element) => Some(self.live.decide(element, *effective_from)?),
                None => None,
            };
            decided.push((reference.clone(), desired));
        }
        for (reference, desired) in decided {
            if row_change(self.published.get(&reference), desired.as_ref(), 0).is_some() {
                self.pending
                    .insert(key(&reference), (reference.clone(), desired));
            } else {
                self.pending.remove(&key(&reference));
            }
        }
        for (reference, effective_from) in touched {
            self.high_water = self.high_water.max(effective_from);
            match staged.remove(&reference).flatten() {
                Some(element) => self.inputs.insert(reference, element),
                None => self.inputs.remove(&reference),
            };
        }
        Ok(())
    }

    /// Re-evaluates every retained input under `live` and returns every row
    /// whose desired decision differs from the published one.
    async fn reevaluate(
        &self,
        live: &Live,
        ctx: &ApplyContext,
    ) -> Result<BTreeMap<Key, (ElementReference, Option<Element>)>, ApplyError> {
        let mut pending = BTreeMap::new();
        for (index, (reference, element)) in self.inputs.iter().enumerate() {
            if index % YIELD_EVERY == YIELD_EVERY - 1 {
                tokio::task::yield_now().await;
                if ctx.cancellation().is_cancelled() {
                    return Err(ApplyError::Failed(anyhow::anyhow!(
                        "policy update was cancelled"
                    )));
                }
            }
            let desired = live
                .decide(element, self.high_water)
                .map_err(ApplyError::Failed)?;
            if row_change(self.published.get(reference), Some(&desired), 0).is_some() {
                pending.insert(key(reference), (reference.clone(), Some(desired)));
            }
        }
        for reference in self.published.keys() {
            if !self.inputs.contains_key(reference) {
                pending.insert(key(reference), (reference.clone(), None));
            }
        }
        Ok(pending)
    }
}

/// Restamps a deferred decision so continuation output never moves
/// backwards behind input that was committed after the update.
fn at(mut element: Element, high_water: u64) -> Element {
    let metadata = match &mut element {
        Element::Node { metadata, .. } | Element::Relation { metadata, .. } => metadata,
    };
    metadata.effective_from = metadata.effective_from.max(high_water);
    element
}

#[async_trait]
impl NativeComponent for PolicyTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.live.config)?)
    }

    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }

    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_snapshot(
            PortId::try_new("out").expect("constant port"),
            self.output.reader(),
        )
    }

    /// The policy was compiled at construction; there is nothing to bind.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }

    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.retired {
            return Err(anyhow::anyhow!("policy state has been deprovisioned").into());
        }
        if self.started {
            return Err(anyhow::anyhow!("policy transformer already started").into());
        }
        if let (Some(source), false) = (self.snapshots.clone(), self.loaded) {
            let mut batch = self
                .upstreams
                .load_all(source.as_ref(), self.high_water)
                .await?;
            self.initialize_from(std::mem::take(&mut batch.changes))?;
            self.upstreams.commit(batch);
            self.loaded = true;
        }
        self.output.set_running(true)?;
        self.started = true;
        self.paused = false;
        Ok(())
    }

    /// Transformation is host-driven and completes before returning, so a
    /// pause settles immediately. Input and continuation are refused while
    /// paused; retained state and pending re-evaluations are kept.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        if self.started {
            self.paused = true;
        }
        Ok(())
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }

    /// Nothing is in flight between host calls, so Drain and Abort are the
    /// same. Retained inputs and decisions survive for a later Start.
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        if !self.retired {
            self.output.set_running(false)?;
        }
        self.started = false;
        self.paused = false;
        Ok(())
    }

    /// `Erase` discards the retained inputs, decisions and pending
    /// re-evaluations and retires the snapshot reader.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if self.started {
            return Err(anyhow::anyhow!("stop the policy transformer before disposing it").into());
        }
        if ctx.erases_owned_state() && !self.retired {
            self.output.retire()?;
            self.inputs.clear();
            self.published.clear();
            self.pending.clear();
            self.retired = true;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for PolicyTransformer {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        let assessment = assess(request)?;
        let proposed = PolicyConfig::parse(request.proposed.configuration.clone())?;
        // Fewer retained inputs than are live cannot be adopted in place.
        if self.inputs.len() as u64 > proposed.max_retained_inputs {
            return Ok(Assessment::supported(
                UpdateBoundary::Replace,
                drasi_computation_contracts::ComponentUpdateData::Preserve,
            ));
        }
        Ok(assessment)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        if self.retired {
            return Err(refused("policy state has been deprovisioned"));
        }
        let current =
            PolicyConfig::parse(update.request.current.configuration.clone()).map_err(refused)?;
        let proposed =
            PolicyConfig::parse(update.request.proposed.configuration.clone()).map_err(refused)?;
        if current != self.live.config {
            return Err(refused(
                "the policy configuration changed after the update was assessed",
            ));
        }
        let assessment = NativeManagement::assess(self, &update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
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
                    return Err(refused("restart updates require a stopped transformer"));
                }
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        // Built and evaluated completely before the swap, so a failure leaves
        // the transformer unchanged.
        let live = Live::new(proposed).map_err(refused)?;
        let pending = self.reevaluate(&live, ctx).await?;
        self.live = live;
        self.pending = pending;
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for PolicyTransformer {
    fn has_pending_emissions(&self) -> bool {
        self.started && !self.paused && !self.retired && !self.pending.is_empty()
    }

    /// Emits up to [`CONTINUATION_BATCH`] re-evaluated decisions that differ
    /// from what downstream has, after a policy update.
    async fn continue_transform(&mut self) -> Result<Vec<OutputEnvelope>> {
        self.ensure_running()?;
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let sequence = self.next_sequence()?;
        let keys: Vec<Key> = self
            .pending
            .keys()
            .take(CONTINUATION_BATCH)
            .cloned()
            .collect();
        let mut changes = Vec::new();
        let mut rows = Vec::new();
        for key in &keys {
            let (reference, desired) = &self.pending[key];
            let desired = desired.clone().map(|element| at(element, self.high_water));
            if let Some(change) = row_change(
                self.published.get(reference),
                desired.as_ref(),
                self.high_water,
            ) {
                changes.push(change);
                rows.push((reference.clone(), desired));
            }
        }
        let envelope = GraphChangeCodec::encode_changes(
            &changes,
            self.live.config.stream.clone(),
            sequence,
            self.last_system
                .as_ref()
                .and_then(|system| system.timestamp()),
        )?;
        let output = self.emit(envelope, sequence, rows)?;
        for key in keys {
            self.pending.remove(&key);
        }
        Ok(vec![output])
    }

    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        self.ensure_running()?;
        ensure!(input.port.as_str() == "in", "unknown policy input port");
        ensure!(
            input.envelope.system().control().is_none(),
            "policy transforms accept graph data, not scheduling or recovery controls"
        );
        let sequence = self.next_sequence()?;
        let decoded = GraphChangeCodec::decode_changes(&input.envelope)?;
        let (changes, batch) = match self.snapshots.clone() {
            Some(source) => {
                let mut batch = self
                    .upstreams
                    .admit(source.as_ref(), &input.envelope, self.high_water)
                    .await?;
                let mut changes = std::mem::take(&mut batch.changes);
                if batch.apply_envelope {
                    batch.include(&decoded);
                    changes.extend(decoded);
                }
                (changes, Some(batch))
            }
            None => (decoded, None),
        };
        let (touched, mut staged) = self.stage(changes)?;
        let mut changes = Vec::new();
        let mut rows = Vec::new();
        for (reference, effective_from) in &touched {
            let desired = match staged.get(reference).and_then(Option::as_ref) {
                Some(element) => Some(self.live.decide(element, *effective_from)?),
                None => None,
            };
            if let Some(change) = row_change(
                self.published.get(reference),
                desired.as_ref(),
                *effective_from,
            ) {
                changes.push(change);
                rows.push((reference.clone(), desired));
            }
        }
        let envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            self.live.config.stream.clone(),
            sequence,
        )?;
        let output = self.emit(envelope, sequence, rows)?;
        for (reference, effective_from) in touched {
            self.pending.remove(&key(&reference));
            self.high_water = self.high_water.max(effective_from);
            match staged.remove(&reference).flatten() {
                Some(element) => self.inputs.insert(reference, element),
                None => self.inputs.remove(&reference),
            };
        }
        if let Some(batch) = batch {
            self.upstreams.commit(batch);
        }
        self.last_system = Some(input.envelope.system().clone());
        Ok(vec![output])
    }
}
