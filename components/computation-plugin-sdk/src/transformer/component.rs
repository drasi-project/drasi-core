// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native component every stateful transformer is.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_computation_contracts::telemetry::{
    ExecutionContext, QueryObservationKind, QueryObservationOutcome,
};
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, CommittedSchedule, ComponentDescriptor,
    ComponentUpdateData, ComponentUpdateMode, GraphChangeCodec, GraphProducerProgress,
    InputBinding, InputEnvelope, OutputCursor, OutputEnvelope, OutputReaders, PortId, WakeupSource,
};

use super::{
    engine::Engine,
    observations::{Observations, Run},
    output::TransformerOutput,
    Options, Phase, Settings, TransformerLogic, INPUT_PORT, OUTPUT_PORT,
};
use crate::{
    lifecycle::{
        nothing_to_do, ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext,
        LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
    },
    ApplyUpdateRequest, BootstrapInput, CreateRequest, NativeBootstrapInput, NativeComponent,
    NativeDurableBootstrapInput, NativeDurableOutput, NativeInputBindings, NativeManagement,
    NativeProcessing, NativeTransformer, UpdateRequest,
};

fn output_port() -> PortId {
    PortId::try_new(OUTPUT_PORT).expect("constant port")
}

/// A transformer whose output is the state it maintains. `L` decides how
/// inputs change that state; everything else is shared.
pub struct StatefulTransformer<L: TransformerLogic> {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    logic: Arc<L>,
    output: Arc<TransformerOutput>,
    // Built once: readers compare providers by identity across republication.
    readers: OutputReaders,
    processing: NativeProcessing,
    engine: Engine<L>,
    started: bool,
    failed: bool,
    schedule: CommittedSchedule,
    draining_futures: bool,
    observations: Observations,
    bootstrap_run: Option<Run>,
    processing_context: ExecutionContext,
}

impl<L: TransformerLogic> StatefulTransformer<L> {
    pub fn new(
        request: &CreateRequest,
        descriptor: ComponentDescriptor,
        processing: NativeProcessing,
        logic: L,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let observations = Observations::capture()?;
        let options = Options::parse(&request.configuration)?;
        let scope = request.scope.as_ref();
        let output = Arc::new(TransformerOutput::new(
            scope
                .map_or("native-standalone", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .map_or("native-standalone", |scope| scope.graph_id.as_str())
                .into(),
            request.id.clone(),
            settings.stream.clone(),
            settings.dataset.as_str().into(),
            settings.capacity,
            observations.clone(),
        ));
        let logic = Arc::new(logic);
        let readers = logic.readers(
            &output,
            OutputReaders::new()
                .with_snapshot(output_port(), output.clone())
                .with_snapshot_follow(output_port(), output.clone())
                .with_replay(output_port(), output.clone()),
        );
        let schedule = CommittedSchedule::default();
        let engine = Engine::new(
            request,
            &settings,
            options,
            output.clone(),
            schedule.clone(),
            observations.clone(),
            logic.clone(),
        )?;
        Ok(Self {
            descriptor,
            configuration: request.configuration.clone(),
            logic,
            output,
            readers,
            processing,
            engine,
            started: false,
            failed: false,
            schedule,
            draining_futures: false,
            observations,
            bootstrap_run: None,
            processing_context: ExecutionContext::default(),
        })
    }

    /// The logic this transformer runs.
    pub fn logic(&self) -> &Arc<L> {
        &self.logic
    }

    /// The materialized output this transformer publishes.
    pub fn output(&self) -> &Arc<TransformerOutput> {
        &self.output
    }

    fn outputs(
        &self,
        outputs: Option<drasi_computation_contracts::ChangeEnvelope>,
    ) -> Vec<OutputEnvelope> {
        outputs
            .into_iter()
            .map(|envelope| OutputEnvelope::new(output_port(), envelope).with_current_context())
            .collect()
    }

    async fn apply_update(&mut self, request: &ApplyUpdateRequest) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "an update requires completed activation and a paused processing boundary"
        );
        let state = self.engine.opened().ok();
        let plan = self
            .logic
            .plan_update(state.as_deref(), &request.request)
            .ok_or_else(|| anyhow::anyhow!("this transformer does not accept updates"))??;
        anyhow::ensure!(
            request.request.current.id == *self.descriptor.id()
                && request.request.current.configuration == self.configuration
                && request.plan == plan
                && request.plan.mode == ComponentUpdateMode::InPlace
                && request.plan.data == ComponentUpdateData::Preserve,
            "retained update does not match the approved configuration and plan"
        );
        let capacity = self.logic.apply_update(request)?;
        self.output.increase_retention(capacity)?;
        self.engine.set_retention(capacity);
        self.configuration = request.request.proposed.configuration.clone();
        Ok(())
    }

    async fn erase_owned_state(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.started, "resource deletion requires completed stop");
        let processing = &self.processing;
        let volatile = self.engine.volatile;
        let non_atomic = self.engine.non_atomic;
        if non_atomic {
            processing
                .run(|context| async move {
                    context.begin_non_atomic_reset(false).await?;
                    Ok(())
                })
                .await?;
        }
        processing
            .run(move |context| async move {
                if volatile {
                    context.reset_volatile_state(false).await?;
                } else if non_atomic {
                    context.reset_non_atomic_state(false).await?;
                } else {
                    context.reset_state(false).await?;
                }
                Ok(())
            })
            .await?;
        if non_atomic {
            processing
                .run(|context| async move {
                    context.finish_non_atomic_reset().await?;
                    Ok(())
                })
                .await?;
        }
        self.output.clear()
    }

    async fn start_activation(&mut self) -> anyhow::Result<()> {
        let observations = self.observations.clone();
        let result = observations
            .transaction(Phase::Bootstrap, async {
                anyhow::ensure!(
                    !self.started,
                    "transformer is already started or initializing"
                );
                let was_failed = self.failed;
                self.output.begin_activation()?;
                self.schedule.publish(None)?;
                self.draining_futures = false;
                self.failed = true;
                self.bootstrap_run = Some(observations.run(Phase::Bootstrap));
                let reset = self.engine.reset_failed_volatile(was_failed)?;
                self.engine.start(&self.processing, reset).await?;
                self.started = true;
                self.failed = false;
                Ok(())
            })
            .await;
        if let Some(run) = self.bootstrap_run.take() {
            run.finish(
                if result.is_ok() {
                    QueryObservationOutcome::Success
                } else {
                    QueryObservationOutcome::Error
                },
                None,
            )?;
        }
        result
    }

    fn stop_activation(&mut self) -> anyhow::Result<()> {
        self.bootstrap_run.take();
        self.processing_context = ExecutionContext::default();
        self.started = false;
        self.draining_futures = false;
        self.schedule.publish(None)?;
        self.output.suspend()?;
        self.engine.stop(self.failed)
    }

    fn verify(
        &self,
    ) -> impl Fn(
        &drasi_computation_contracts::ChangeEnvelope,
        &drasi_computation_contracts::GraphProducerIdentity,
        u64,
    ) -> anyhow::Result<()>
           + Sync
           + '_ {
        |envelope, producer, generation| self.logic.verify(envelope, producer, generation)
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeComponent for StatefulTransformer<L> {
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        if self.logic.manages_updates() {
            Some(self)
        } else {
            None
        }
    }

    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    fn output_readers(&self) -> OutputReaders {
        self.readers.clone()
    }

    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }

    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        self.engine.unconnected_inputs = ctx.unconnected_inputs();
        self.engine.state_subscribers = ctx.state_subscribers();
        Ok(self.start_activation().await?)
    }

    // Transformation, continuations and wakeups are driven by the host; the
    // transformer owns no independent workers.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        nothing_to_do()
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        nothing_to_do()
    }

    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        Ok(self.stop_activation()?)
    }

    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if ctx.erases_owned_state() {
            self.erase_owned_state().await?;
        }
        Ok(())
    }

    fn input_bindings(&mut self) -> Option<&mut dyn NativeInputBindings> {
        Some(self)
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeManagement for StatefulTransformer<L> {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        let state = self.engine.opened().ok();
        match self.logic.plan_update(state.as_deref(), request) {
            Some(plan) => plan.map(Assessment::from),
            None => anyhow::bail!("this transformer does not accept updates"),
        }
    }

    async fn apply(
        &mut self,
        _ctx: &ApplyContext,
        request: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        Ok(self.apply_update(request).await?)
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeTransformer for StatefulTransformer<L> {
    fn has_pending_emissions(&self) -> bool {
        self.draining_futures || self.engine.has_pending_emissions()
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.started && !self.failed,
            "transformer is not ready for continuation"
        );
        if !self.engine.has_pending_emissions() {
            return if self.draining_futures {
                self.on_wakeup().await
            } else {
                Ok(Vec::new())
            };
        }
        let run = self.observations.run(Phase::Steady);
        run.start(QueryObservationKind::OutputReplay)?;
        self.failed = true;
        let output = run
            .scope(self.engine.continue_output(&self.processing))
            .await;
        self.processing_context = run.context()?.unwrap_or_default();
        run.finish(
            if output.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            Some(u64::from(output.as_ref().is_ok_and(Option::is_some))),
        )?;
        let output = output?;
        self.failed = false;
        Ok(output
            .into_iter()
            .map(|envelope| {
                OutputEnvelope::new(output_port(), envelope)
                    .with_execution_context(self.processing_context.clone())
            })
            .collect())
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let observations = self.observations.clone();
        observations
            .transaction(Phase::Steady, async {
                anyhow::ensure!(
                    !self.failed,
                    "transformer is fenced after a processing failure"
                );
                anyhow::ensure!(self.started, "transformer is not started");
                anyhow::ensure!(
                    input.port.as_str() == INPUT_PORT,
                    "invalid transformer input"
                );
                let futures_due = self.engine.source_transactions.is_none()
                    && GraphChangeCodec::is_futures_due(&input.envelope);
                if futures_due {
                    let progress = GraphProducerProgress::from_envelope(&input.envelope)?;
                    if progress.as_ref().is_none_or(|progress| {
                        !progress.identity().persistent() && self.engine.volatile
                    }) {
                        return self.on_wakeup().await;
                    }
                }
                let scope = self.logic.scope(&input, Phase::Steady)?;
                self.failed = true;
                let publication = self.output.processing();
                let output = self
                    .engine
                    .transform(&self.processing, input, scope)
                    .await?;
                if let Some(output) = &output {
                    self.output.publish(output, &self.verify())?;
                }
                publication.complete();
                self.failed = false;
                if futures_due {
                    return self.on_wakeup().await;
                }
                Ok(self.outputs(output))
            })
            .await
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.schedule.wakeup_source())
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.processing_context = ExecutionContext::default();
        anyhow::ensure!(
            self.started && !self.failed,
            "transformer is not ready for scheduled work"
        );
        if self.engine.has_pending_emissions() {
            return self.continue_transform().await;
        }
        let observations = self.observations.clone();
        let run = observations.run(Phase::Steady);
        let result: anyhow::Result<Vec<OutputEnvelope>> = observations
            .transaction(Phase::Steady, async {
                self.failed = true;
                let publication = self.output.processing();
                let (output, now) = self.engine.on_wakeup(&self.processing, run.clone()).await?;
                run.scope(async {
                    if let Some(output) = &output {
                        self.output.publish(output, &self.verify())?;
                    }
                    self.draining_futures = self
                        .schedule
                        .next_due()?
                        .is_some_and(|deadline| deadline <= now);
                    publication.complete();
                    self.failed = false;
                    Ok(self.outputs(output))
                })
                .await
            })
            .await;
        self.processing_context = run.context()?.unwrap_or_default();
        run.finish(
            if result.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            result.as_ref().ok().map(|outputs| outputs.len() as u64),
        )?;
        Ok(result?
            .into_iter()
            .map(|output| output.with_execution_context(self.processing_context.clone()))
            .collect())
    }

    fn bootstrap_input(&mut self) -> Option<BootstrapInput<'_>> {
        Some(BootstrapInput::Durable(self))
    }

    fn durable_output(&mut self) -> Option<&mut dyn NativeDurableOutput> {
        Some(self)
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeInputBindings for StatefulTransformer<L> {
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        self.logic.bind_inputs(bindings)
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeDurableOutput for StatefulTransformer<L> {
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "transformer is not ready for confirmation"
        );
        self.failed = true;
        self.observations
            .transaction(
                Phase::Steady,
                self.engine.delivery_completed(&self.processing, outputs),
            )
            .await?;
        self.failed = false;
        Ok(())
    }

    fn resume_delivery(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.started && !self.failed,
            "transformer is not ready for delivery resumption"
        );
        self.engine.resume_delivery();
        Ok(())
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeBootstrapInput for StatefulTransformer<L> {
    fn bootstrap_status(&self) -> anyhow::Result<BootstrapStatus> {
        Ok(self.engine.bootstrap_status())
    }

    async fn bootstrap(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let observations = self.observations.clone();
        let run = self
            .bootstrap_run
            .get_or_insert_with(|| observations.run(Phase::Bootstrap))
            .clone();
        run.start(QueryObservationKind::Bootstrap)?;
        let result = run
            .scope(async {
                anyhow::ensure!(!self.failed, "transformer initialization is fenced");
                anyhow::ensure!(
                    input.port.as_str() == INPUT_PORT,
                    "invalid transformer bootstrap input"
                );
                let scope = self.logic.scope(&input, Phase::Bootstrap)?;
                self.failed = true;
                self.engine
                    .bootstrap(&self.processing, input, scope)
                    .await?;
                self.failed = false;
                Ok(())
            })
            .await;
        if result.is_err() {
            run.finish(QueryObservationOutcome::Error, None)?;
            self.bootstrap_run.take();
        }
        result
    }

    async fn bootstrap_completed(&mut self, completion: BootstrapCompletion) -> anyhow::Result<()> {
        let observations = self.observations.clone();
        let needs_initialization = !self.engine.bootstrap_status().complete;
        let run = self
            .bootstrap_run
            .get_or_insert_with(|| observations.run(Phase::Bootstrap))
            .clone();
        if needs_initialization {
            run.start(QueryObservationKind::Bootstrap)?;
        }
        let result = run
            .scope(async {
                anyhow::ensure!(!self.failed, "transformer initialization is fenced");
                self.failed = true;
                self.engine
                    .complete_or_rebuild(&self.processing, completion)
                    .await?;
                self.started = true;
                self.failed = false;
                Ok(())
            })
            .await;
        run.finish(
            if result.is_ok() {
                QueryObservationOutcome::Success
            } else {
                QueryObservationOutcome::Error
            },
            None,
        )?;
        self.bootstrap_run.take();
        result
    }
}

#[async_trait]
impl<L: TransformerLogic> NativeDurableBootstrapInput for StatefulTransformer<L> {
    fn begin_rebuild(&mut self, after: Option<OutputCursor>) -> anyhow::Result<bool> {
        anyhow::ensure!(!self.failed, "transformer initialization is fenced");
        self.engine.begin_rebuild(after)
    }

    async fn reset_bootstrap(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "transformer initialization is fenced");
        self.failed = true;
        self.engine.start(&self.processing, true).await?;
        self.failed = false;
        Ok(())
    }
}
