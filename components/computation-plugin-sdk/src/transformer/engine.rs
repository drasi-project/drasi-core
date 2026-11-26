// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The processing engine of a stateful transformer: durable initialization
//! and recovery, bootstrap, steady processing, scheduled work, restart replay
//! and delivery confirmation. Every state change commits in the same host
//! operation as the output and input progress it implies.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use drasi_computation_contracts::profiling::timestamp_ns;
use drasi_computation_contracts::telemetry::QueryObservationKind;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ChangeEnvelope, CommittedSchedule, GraphChangeCodec,
    GraphProducerIdentity, InputEnvelope, OutputCursor, OutputEnvelope, QueryRecoveryError,
    QueryRecoveryPolicy as RecoveryPolicy, SourceProgressKey, SourceTransactionLimits, StreamId,
    BOOTSTRAP_COMPLETE_CHECKPOINT, BOOTSTRAP_STATE_CHECKPOINT,
};
use drasi_core_types::interface::{FailureMode, OutboxPageLimits, RowMutation};

use super::{
    logical_sequence, non_atomic, observations::Observations, output::TransformerOutput, rebuild,
    recovery, Options, Phase, Rows, Settings, TransformerLogic,
};
use crate::{CreateRequest, NativeProcessing, NativeProcessingContext};

pub(super) const CONFIGURATION: &str = "\0transformer:configuration:v1";
pub(super) const UPDATE_OPERATION: &str = "\0transformer:update-operation:v1";
pub(super) const PRODUCER: &str = "\0transformer:producer:v1";
pub(super) const EMISSION: &str = "\0transformer:emission:v1";
pub(super) const ROW_COUNT: &str = "\0transformer:live-row-count:v1";
pub(super) const VOLATILE_DELIVERED: &str = "\0transformer:volatile-delivered:v1";
pub(super) const AWAITING_FIRST_INPUT: &str = "\0transformer:awaiting-first-input:v1";

#[derive(serde::Serialize, serde::Deserialize)]
enum RecoveryAttempt {
    Ready(GraphProducerIdentity, u64, u64, bool, bool),
    ResetRequired(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FirstInput {
    None,
    /// Read the first input as this view's initial state, in a new epoch.
    Initialize,
    /// A dependent installed the empty view; publish the first input's rows.
    Rebuild,
}

pub(super) struct Engine<L: TransformerLogic> {
    pub(super) logic: Arc<L>,
    pub(super) volatile: bool,
    pub(super) non_atomic: bool,
    pub(super) initialized: bool,
    pub(super) configuration: Vec<u8>,
    pub(super) producer: GraphProducerIdentity,
    pub(super) sequence: u64,
    pub(super) delivered: u64,
    pub(super) pending: Option<ChangeEnvelope>,
    pub(super) replay: bool,
    pub(super) output: Arc<TransformerOutput>,
    pub(super) bootstrap_configured: bool,
    pub(super) bootstrap_complete: bool,
    pub(super) rebuild: Option<rebuild::Rebuild>,
    pub(super) state_subscribers: bool,
    pub(super) first_input_rebuild: bool,
    pub(super) unconnected_inputs: bool,
    pub(super) source_transactions: Option<SourceTransactionLimits>,
    pub(super) recovery: RecoveryPolicy,
    pub(super) reset_configuration: bool,
    pub(super) update_operation: Option<String>,
    pub(super) state: Option<Arc<L::State>>,
    pub(super) capacity: NonZeroUsize,
    pub(super) stream: StreamId,
    pub(super) schedule: CommittedSchedule,
    pub(super) observations: Observations,
}

pub(super) fn page_limits() -> OutboxPageLimits {
    OutboxPageLimits {
        max_records: NonZeroUsize::new(1).expect("one retained output"),
        max_bytes: NonZeroUsize::new(1 << 20).expect("positive page bound"),
    }
}

pub(super) fn row_page_limits() -> OutboxPageLimits {
    OutboxPageLimits {
        max_records: NonZeroUsize::new(256).expect("positive row bound"),
        max_bytes: NonZeroUsize::new(1 << 20).expect("positive page bound"),
    }
}

pub(super) fn codec() -> Result<drasi_computation_contracts::BinaryEnvelopeCodec> {
    crate::wire::codec(&[GraphChangeCodec::schema()])
}

pub(super) fn decode(bytes: Option<Vec<u8>>) -> Result<Option<ChangeEnvelope>> {
    bytes.map(|bytes| Ok(codec()?.decode(&bytes)?)).transpose()
}

pub(super) async fn number(context: &NativeProcessingContext, key: &str) -> Result<u64> {
    let value = context
        .read_checkpoint(key)
        .await?
        .with_context(|| format!("missing transformer checkpoint {key:?}"))?;
    anyhow::ensure!(
        value.source_position.is_none(),
        "invalid transformer counter {key:?}"
    );
    Ok(value.sequence)
}

pub(super) async fn delivered_sequence(
    context: &NativeProcessingContext,
    volatile: bool,
) -> Result<u64> {
    if volatile {
        number(context, VOLATILE_DELIVERED).await
    } else {
        context
            .read_delivery_progress()
            .await?
            .context("missing transformer delivery checkpoint")
    }
}

/// Run one operation with the logic's private stores bound to it.
pub(super) fn bound<L: TransformerLogic>(
    logic: &L,
    context: NativeProcessingContext,
) -> Result<(Arc<NativeProcessingContext>, Box<dyn Send>)> {
    let context = Arc::new(context);
    let binding = logic.bind(&context)?;
    Ok((context, binding))
}

// A completed view of zero configured inputs is not a completed read of an
// empty producer. Only the former may enter its first real initialization.
pub(super) async fn first_input_boundary(
    context: &NativeProcessingContext,
    configured: bool,
    unconnected: bool,
    fresh: bool,
    volatile: bool,
    defer: bool,
) -> Result<FirstInput> {
    if !configured {
        return Ok(FirstInput::None);
    }
    if fresh {
        if unconnected {
            context
                .stage_checkpoint(AWAITING_FIRST_INPUT, 1, None)
                .await?;
        }
        return Ok(FirstInput::None);
    }
    let Some(marker) = context.read_checkpoint(AWAITING_FIRST_INPUT).await? else {
        return Ok(FirstInput::None);
    };
    anyhow::ensure!(
        marker.sequence <= 1 && marker.source_position.is_none(),
        "invalid first-input marker"
    );
    if marker.sequence == 0 || unconnected {
        return Ok(FirstInput::None);
    }
    anyhow::ensure!(
        context.read_result_sequence().await? == Some(0)
            && delivered_sequence(context, volatile).await? == 0
            && number(context, EMISSION).await? == 0
            && number(context, ROW_COUNT).await? == 0
            && number(context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 1
            && context
                .read_checkpoint(BOOTSTRAP_STATE_CHECKPOINT)
                .await?
                .is_none(),
        "first input cannot replace previously initialized transformer state"
    );
    if defer {
        // Keep the installed empty view and its epoch. The rebuild commits
        // the first rows as a change that a subscriber at this cursor follows.
        return Ok(FirstInput::Rebuild);
    }
    context
        .stage_checkpoint(AWAITING_FIRST_INPUT, 0, None)
        .await?;
    context.stage_bootstrap_complete(false).await?;
    Ok(FirstInput::Initialize)
}

/// Store the live rows for `rows`, the rows an output leaves.
pub(super) async fn apply_live_rows<L: TransformerLogic>(
    context: &NativeProcessingContext,
    logic: &L,
    output: &ChangeEnvelope,
    rows: &Rows,
) -> Result<()> {
    let mutations = logic.row_mutations(output, rows)?;
    let mutations: Vec<_> = mutations
        .iter()
        .map(|(key, data)| RowMutation {
            row_signature: *key,
            data: data.as_deref(),
        })
        .collect();
    context.apply_live_rows(&mutations).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn stage_output<L: TransformerLogic>(
    context: &NativeProcessingContext,
    logic: &L,
    state: &TransformerOutput,
    mut output: ChangeEnvelope,
    sequence: u64,
    retention: u64,
    volatile: bool,
    deferred: bool,
) -> Result<Vec<u8>> {
    anyhow::ensure!(
        output.system().sequence() == sequence && output.system().stream() == state.stream(),
        "transformer output does not carry its stream position"
    );
    let codec = codec()?;
    if deferred {
        non_atomic::stage(context, non_atomic::Publication::Pending(sequence)).await?;
        return Ok(codec.encode(&output)?);
    }
    state.stamp(&mut output, sequence, &|envelope, producer, generation| {
        logic.decorate(envelope, producer, generation)
    })?;
    let rows = state.rows_after(&output)?;
    if !volatile {
        apply_live_rows(context, logic, &output, &rows).await?;
    }
    context
        .stage_checkpoint(ROW_COUNT, u64::try_from(rows.len())?, None)
        .await?;
    if !volatile {
        context
            .append_outbox_and_trim(
                sequence,
                &codec.encode(&output)?,
                sequence.saturating_sub(retention - 1),
            )
            .await?;
    }
    context.stage_result_sequence(sequence).await?;
    let emission = context
        .next_emission_sequence(number(context, EMISSION).await?)
        .await?;
    context.stage_checkpoint(EMISSION, emission, None).await?;
    Ok(codec.encode(&output.reemit(emission)?)?)
}

impl<L: TransformerLogic> Engine<L> {
    pub(super) fn set_retention(&mut self, capacity: NonZeroUsize) {
        self.capacity = capacity;
    }

    pub(super) fn new(
        request: &CreateRequest,
        settings: &Settings,
        options: Options,
        output: Arc<TransformerOutput>,
        schedule: CommittedSchedule,
        observations: Observations,
        logic: Arc<L>,
    ) -> Result<Self> {
        let scope = request
            .scope
            .as_ref()
            .context("a stateful transformer requires a host scope")?;
        let Options {
            bootstrap: bootstrap_configured,
            volatile,
            non_atomic,
            recovery,
            reset_configuration,
        } = options;
        anyhow::ensure!(
            !(volatile || non_atomic) || settings.source_transactions.is_none(),
            "complete source transactions require atomic publication"
        );
        anyhow::ensure!(
            recovery != RecoveryPolicy::AutoReset || bootstrap_configured,
            "AutoReset requires configured provider bootstrap"
        );
        let mut configuration = serde_json::json!({
            "state_format": 9,
            "implementation": request.implementation,
            "configuration": settings.identity,
            "bootstrap": bootstrap_configured,
            "instance": scope.instance_id,
            "graph": scope.graph_id,
            "component": request.id,
            "stream": settings.stream,
        });
        if non_atomic {
            configuration["publication"] = serde_json::json!("non_atomic");
        }
        let configuration = serde_json::to_vec(&configuration)?;
        Ok(Self {
            logic,
            volatile,
            non_atomic,
            initialized: false,
            configuration,
            producer: GraphProducerIdentity::new(
                scope.instance_id.clone(),
                scope.graph_id.clone(),
                request.id.clone(),
                settings.stream.clone(),
                !volatile,
            )?,
            sequence: 0,
            delivered: 0,
            pending: None,
            replay: false,
            output,
            bootstrap_configured,
            bootstrap_complete: false,
            rebuild: None,
            state_subscribers: false,
            first_input_rebuild: false,
            unconnected_inputs: false,
            source_transactions: settings.source_transactions,
            recovery,
            reset_configuration,
            update_operation: request
                .update_directive
                .as_ref()
                .map(|directive| directive.operation_id.clone()),
            state: None,
            capacity: settings.capacity,
            stream: settings.stream.clone(),
            schedule,
            observations,
        })
    }

    pub(super) async fn start(
        &mut self,
        processing: &NativeProcessing,
        force_reset: bool,
    ) -> Result<()> {
        if self.volatile {
            return self.start_volatile(processing, force_reset).await;
        }
        anyhow::ensure!(
            force_reset || self.state.is_none(),
            "transformer state is already open"
        );
        if self.non_atomic {
            Box::pin(self.prepare_non_atomic_reset(processing, force_reset)).await?;
        }
        let existing_state = self.state.take();
        let logic = self.logic.clone();
        let configuration = self.configuration.clone();
        let candidate = self.producer.clone();
        let restored = Arc::new(Mutex::new(None));
        let staged_rows = restored.clone();
        let output = self.output.clone();
        let bootstrap_configured = self.bootstrap_configured;
        let unconnected = self.unconnected_inputs;
        let recovery = self.recovery;
        let reset_configuration = self.reset_configuration;
        let update_operation = self.update_operation.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let non_atomic = self.non_atomic;
        // The same eligibility as a gap rebuild, which installs these rows.
        let defer_first_input = self.state_subscribers
            && !non_atomic
            && self.source_transactions.is_none()
            && self.logic.rebuild_supported();
        let attempt: RecoveryAttempt = processing
            .run(move |context| async move {
                let (context, _binding) = bound(&*logic, context)?;
                let mut identified_read_only_state = false;
                let result: Result<(GraphProducerIdentity, u64, u64, bool, bool)> = async {
                let profile = context.state_info().await?;
                anyhow::ensure!(
                    !profile.volatile && profile.non_atomic_publication == non_atomic
                        && profile.persistent_checkpoints,
                    "transformer publication does not match its actual persistent profile"
                );
                if !non_atomic {
                    context.atomic_state_info().await?.durability.require(FailureMode::ProcessRestart)?;
                }
                let reset_plan = recovery::reset_if_required(
                    &context,
                    &configuration,
                    &candidate,
                    recovery::ResetIntent {
                        policy: recovery,
                        bootstrap_configured,
                        unconnected,
                        force: force_reset,
                        reset_configuration,
                        update_operation: update_operation.as_deref(),
                    },
                    &|output, producer| logic.verify_owner(output, producer),
                )
                .await?;
                let candidate = if reset_plan.is_some() {
                    GraphProducerIdentity::new(
                        candidate.construction_scope().to_owned(),
                        candidate.graph_id().to_owned(),
                        candidate.component_id().clone(),
                        candidate.stream().clone(),
                        true,
                    )?
                } else {
                    candidate
                };
                let saved = context.read_checkpoint(CONFIGURATION).await?;
                let new_state = saved.is_none();
                let cleared_state = new_state && reset_plan.is_none() && context.reset_generation().await?.is_some();
                let sequence = if reset_plan.is_some() {
                    // This counter reads committed storage, not the staged reset.
                    None
                } else {
                    context.read_result_sequence().await?
                };
                let producer = context.read_checkpoint(PRODUCER).await?;
                let delivered = context.read_delivery_progress().await?;
                let emission = context.read_checkpoint(EMISSION).await?;
                let count = context.read_checkpoint(ROW_COUNT).await?;
                let bootstrap = context
                    .read_checkpoint(BOOTSTRAP_COMPLETE_CHECKPOINT)
                    .await?;
                let bootstrap_complete = match &bootstrap {
                    Some(marker) => {
                        anyhow::ensure!(
                            marker.sequence <= 1 && marker.source_position.is_none(),
                            "invalid transformer bootstrap marker"
                        );
                        marker.sequence == 1
                    }
                    None if new_state => !bootstrap_configured || unconnected,
                    None => anyhow::bail!("missing committed transformer bootstrap marker"),
                };
                anyhow::ensure!(
                    new_state || bootstrap_complete,
                    "incomplete durable bootstrap requires explicit recovery/reset"
                );
                let (producer, sequence, delivered) = if let Some(saved) = saved {
                    anyhow::ensure!(
                        saved.sequence == 1
                            && saved.source_position.as_deref() == Some(configuration.as_slice()),
                        "transformer configuration changed; retained state cannot be reused"
                    );
                    let producer = recovery::producer(
                        producer.context("missing persisted transformer producer")?,
                        &candidate,
                    )?;
                    let sequence = sequence.context("missing transformer result sequence")?;
                    let delivered = delivered.context("missing transformer delivery checkpoint")?;
                    let emission = emission.context("missing transformer emission checkpoint")?;
                    anyhow::ensure!(
                        emission.source_position.is_none()
                            && delivered <= sequence
                            && sequence - delivered <= 1
                            && emission.sequence >= sequence,
                        "invalid transformer output/delivery progress"
                    );
                    identified_read_only_state = reset_plan.is_none();
                    (producer, sequence, delivered)
                } else {
                    anyhow::ensure!(
                        (sequence.is_none() || cleared_state && sequence == Some(0))
                            && producer.is_none()
                            && delivered.is_none()
                            && emission.is_none()
                            && count.is_none()
                            && bootstrap.is_none()
                            && (reset_plan.is_some()
                                || (context
                                    .read_live_rows_page(None, row_page_limits())
                                    .await?
                                    .is_empty()
                                    && context
                                        .read_outbox_page(0, page_limits())
                                        .await?
                                        .is_empty())),
                        "unidentified retained transformer state (sequence={sequence:?}, producer={}, delivered={}, emission={}, rows={}, bootstrap={}, resetting={})",
                        producer.is_some(), delivered.is_some(), emission.is_some(), count.is_some(),
                        bootstrap.is_some(), reset_plan.is_some()
                    );
                    context
                        .stage_checkpoint(CONFIGURATION, 1, Some(&configuration))
                        .await?;
                    if let Some(operation) = &update_operation {
                        context
                            .stage_checkpoint(UPDATE_OPERATION, 1, Some(operation.as_bytes()))
                            .await?;
                    }
                    context
                        .stage_checkpoint(PRODUCER, 1, Some(&serde_json::to_vec(&candidate)?))
                        .await?;
                    let sequence = reset_plan.map_or(0, |plan| plan.sequence);
                    if reset_plan.is_some() {
                        context.initialize_reset_output(sequence).await?;
                        recovery::stage_baseline(&context, sequence).await?;
                    } else {
                        context.stage_result_sequence(sequence).await?;
                        context.stage_delivery_progress(sequence).await?;
                    }
                    context
                        .stage_checkpoint(EMISSION, reset_plan.map_or(0, |plan| plan.emission), None)
                        .await?;
                    context.stage_checkpoint(ROW_COUNT, 0, None).await?;
                    context.stage_bootstrap_complete(bootstrap_complete).await?;
                    (candidate, sequence, sequence)
                };
                let first_input = first_input_boundary(
                    &context, bootstrap_configured, unconnected, new_state, false, defer_first_input,
                ).await?;
                let (producer, bootstrap_complete) = if first_input == FirstInput::Initialize {
                    // A reader may already hold a cut of the disconnected
                    // output. Its cursor cannot name the newly seeded view.
                    let producer = GraphProducerIdentity::new(
                        producer.construction_scope().to_owned(), producer.graph_id().to_owned(),
                        producer.component_id().clone(), producer.stream().clone(), true,
                    )?;
                    context.stage_checkpoint(PRODUCER, 1, Some(&serde_json::to_vec(&producer)?)).await?;
                    (producer, false)
                } else {
                    (producer, bootstrap_complete)
                };
                let expected_rows = match count {
                    Some(count) => {
                        anyhow::ensure!(
                            count.source_position.is_none(),
                            QueryRecoveryError::Inconsistent("invalid stored row count".into())
                        );
                        count.sequence
                    }
                    None if new_state => 0,
                    None => anyhow::bail!(QueryRecoveryError::Inconsistent(
                        "missing committed row count".into()
                    )),
                };
                let mut rows = Rows::new();
                let mut after = None;
                loop {
                    // Paging reads committed storage; a successful staged reset
                    // initializes an empty projection in this transaction instead.
                    if new_state {
                        break;
                    }
                    let page = context
                        .read_live_rows_page(after, row_page_limits())
                        .await?;
                    if page.is_empty() {
                        break;
                    }
                    for (signature, payload) in page {
                        let record = logic.restore_row(signature, payload.into()).with_context(|| {
                            QueryRecoveryError::Inconsistent(format!(
                                "failed to deserialize durable live row {signature}"
                            ))
                        })?;
                        anyhow::ensure!(
                            rows.insert(record.identity().clone(), record).is_none(),
                            QueryRecoveryError::Inconsistent("duplicate stored row".into())
                        );
                        anyhow::ensure!(
                            u64::try_from(rows.len())? <= expected_rows,
                            QueryRecoveryError::Inconsistent("stored rows exceed the committed row count".into())
                        );
                        after = Some(signature);
                    }
                }
                anyhow::ensure!(
                    u64::try_from(rows.len())? == expected_rows,
                    QueryRecoveryError::Inconsistent("stored rows do not match the committed row count".into())
                );
                let codec = codec()?;
                let mut retained = Vec::new();
                if sequence == 0 || new_state {
                    anyhow::ensure!(
                        reset_plan.is_some()
                            || context.read_outbox_page(0, page_limits()).await?.is_empty(),
                        QueryRecoveryError::Inconsistent("output exists without a committed sequence".into())
                    );
                } else {
                    let mut after = sequence.saturating_sub(retention);
                    loop {
                        let page = context.read_outbox_page(after, row_page_limits()).await?;
                        if page.is_empty() {
                            break;
                        }
                        for (stored_sequence, bytes) in page {
                            anyhow::ensure!(
                                stored_sequence > after && stored_sequence <= sequence
                                    && (retained.is_empty() || stored_sequence == after + 1),
                                QueryRecoveryError::Inconsistent("retained history is not contiguous".into())
                            );
                            let envelope = codec.decode(&bytes).with_context(|| {
                                QueryRecoveryError::Inconsistent(format!(
                                    "failed to deserialize durable outbox entry {stored_sequence}"
                                ))
                            })?;
                            anyhow::ensure!(
                                logical_sequence(&envelope).with_context(|| {
                                    QueryRecoveryError::Inconsistent(format!(
                                        "durable outbox entry {stored_sequence} has no position"
                                    ))
                                })? == stored_sequence,
                                QueryRecoveryError::Inconsistent("retained output does not match committed progress".into())
                            );
                            retained.push(envelope);
                            after = stored_sequence;
                        }
                        if after == sequence {
                            break;
                        }
                    }
                }
                let generation = context.reset_generation().await?.unwrap_or(0);
                let baseline = if let Some(marker) = recovery::baseline(&context).await? {
                    anyhow::ensure!(
                        marker.generation == generation && marker.sequence <= sequence,
                        "reset baseline disagrees with committed transformer state"
                    );
                    marker.sequence == sequence && retained.is_empty()
                } else {
                    false
                };
                output.validate_producer(&producer)?;
                recovery::validate(
                    &OutputCursor::try_new(producer.clone(), sequence)?,
                    generation,
                    &rows,
                    &retained,
                    baseline,
                    &|envelope, producer, generation| logic.verify(envelope, producer, generation),
                )?;
                let state = if let Some(state) = existing_state {
                    anyhow::ensure!(
                        reset_plan.is_some(),
                        "retained transformer state requires an owned bootstrap reset"
                    );
                    // Reset in-memory state with the same owned storage reset.
                    logic.reset(&state).await?;
                    state
                } else {
                    logic.open(&context).await?
                };
                context.recover_input_progress().await?;
                *staged_rows
                    .lock()
                    .map_err(|_| anyhow::anyhow!("transformer hydration state poisoned"))? =
                    Some((rows, state, retained, generation));
                Ok((producer, sequence, delivered, bootstrap_complete, first_input == FirstInput::Rebuild))
                }.await;
                match result {
                    Ok((producer, sequence, delivered, complete, first_input)) =>
                        Ok(RecoveryAttempt::Ready(producer, sequence, delivered, complete, first_input)),
                    Err(error) if !force_reset && identified_read_only_state && bootstrap_configured
                        && recovery::should_reset(&error, recovery, reset_configuration) =>
                        Ok(RecoveryAttempt::ResetRequired(format!("{error:#}"))),
                    Err(error) => Err(error),
                }
            })
            .await?;
        let (producer, sequence, delivered, bootstrap_complete, first_input_rebuild) = match attempt
        {
            RecoveryAttempt::Ready(producer, sequence, delivered, complete, first_input) => {
                (producer, sequence, delivered, complete, first_input)
            }
            RecoveryAttempt::ResetRequired(reason) => {
                log::warn!(
                    "Rebuilding transformer '{}' under AutoReset after retained-state validation failed: {reason}",
                    self.producer.component_id(),
                );
                return Box::pin(self.start(processing, true)).await;
            }
        };
        let (rows, state, retained, generation) = restored
            .lock()
            .map_err(|_| anyhow::anyhow!("transformer hydration state poisoned"))?
            .take()
            .context("transformer hydration did not complete")?;
        self.pending = recovery::pending_outputs(&retained, sequence, delivered)?.pop_front();
        self.replay = self.pending.is_some();
        self.producer = producer;
        self.sequence = sequence;
        self.delivered = delivered;
        self.bootstrap_complete = bootstrap_complete;
        self.first_input_rebuild = first_input_rebuild;
        let logic = self.logic.clone();
        self.output.restore(
            self.producer.clone(),
            self.sequence,
            rows,
            false,
            retained,
            generation,
            &|envelope, producer, generation| logic.verify(envelope, producer, generation),
        )?;
        self.state = Some(state);
        if self.bootstrap_complete {
            self.refresh_schedule(processing).await?;
        } else {
            self.schedule.publish(None)?;
        }
        Ok(())
    }

    pub(super) fn reset_failed_volatile(&self, failed: bool) -> Result<bool> {
        if self.volatile && failed {
            anyhow::ensure!(
                self.recovery == RecoveryPolicy::AutoReset && self.bootstrap_configured,
                "failed volatile transformer requires explicit reconstruction and bootstrap"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(super) fn opened(&self) -> Result<Arc<L::State>> {
        self.state.clone().context("transformer state is not open")
    }

    pub(super) async fn refresh_schedule(&self, processing: &NativeProcessing) -> Result<()> {
        let logic = self.logic.clone();
        let state = self.opened()?;
        let next_due = processing
            .run(move |context| async move {
                let (_context, _binding) = bound(&*logic, context)?;
                logic.next_due(&state).await
            })
            .await?;
        self.logic.take_schedule_change();
        self.schedule.publish(next_due)
    }

    pub(super) fn bootstrap_status(&self) -> BootstrapStatus {
        BootstrapStatus {
            configured: self.bootstrap_configured,
            complete: self.bootstrap_complete && self.rebuild.is_none(),
            reset_allowed: self.recovery == RecoveryPolicy::AutoReset,
        }
    }

    pub(super) async fn bootstrap(
        &mut self,
        processing: &NativeProcessing,
        input: InputEnvelope,
        input_scope: Option<drasi_computation_contracts::ComponentId>,
    ) -> Result<()> {
        if let Some(rebuild) = &mut self.rebuild {
            rebuild.push(input, input_scope)?;
            return self.observations.input(Phase::Bootstrap);
        }
        anyhow::ensure!(
            self.bootstrap_configured && !self.bootstrap_complete,
            "transformer is not accepting initialization"
        );
        let output = self.output.clone();
        let volatile = self.volatile;
        let non_atomic = self.non_atomic;
        let sequence = self.sequence;
        let logic = self.logic.clone();
        let state = self.opened()?;
        let observations = self.observations.clone();
        let bytes: Option<Vec<u8>> = processing
            .run(move |context| async move {
                let (context, _binding) = bound(&*logic, context)?;
                anyhow::ensure!(
                    number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 0
                        && context.read_result_sequence().await? == Some(sequence),
                    "bootstrap cannot replace a live output generation"
                );
                let mut prepared =
                    logic.prepare(&input.envelope, input_scope, None, Phase::Bootstrap)?;
                let evaluation = logic
                    .evaluate(&state, &mut prepared, Phase::Bootstrap, &observations)
                    .await?;
                let Some(envelope) =
                    logic.project(&input.envelope, prepared, evaluation, sequence)?
                else {
                    return Ok(None);
                };
                anyhow::ensure!(
                    number(&context, ROW_COUNT).await? == output.row_count()?,
                    "bootstrap projection does not match committed rows"
                );
                if !non_atomic {
                    let rows = output.rows_after(&envelope)?;
                    if !volatile {
                        apply_live_rows(&context, &*logic, &envelope, &rows).await?;
                    }
                    context
                        .stage_checkpoint(ROW_COUNT, u64::try_from(rows.len())?, None)
                        .await?;
                }
                Ok(Some(codec()?.encode(&envelope)?))
            })
            .await?;
        if let Some(bytes) = bytes {
            let envelope = codec()?.decode(&bytes)?;
            if non_atomic {
                let projection = envelope.clone();
                let state = self.output.clone();
                let logic = self.logic.clone();
                processing
                    .run(move |context| async move {
                        anyhow::ensure!(
                            number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 0
                                && context.read_result_sequence().await? == Some(sequence)
                                && number(&context, ROW_COUNT).await? == state.row_count()?,
                            "bootstrap projection changed before non-atomic publication"
                        );
                        let rows = state.rows_after(&projection)?;
                        apply_live_rows(&context, &*logic, &projection, &rows).await?;
                        context
                            .stage_checkpoint(ROW_COUNT, u64::try_from(rows.len())?, None)
                            .await?;
                        Ok(())
                    })
                    .await?;
            }
            self.output.apply_bootstrap(&envelope)?;
        }
        self.observations.input(Phase::Bootstrap)?;
        Ok(())
    }

    pub(super) async fn complete_bootstrap(
        &mut self,
        processing: &NativeProcessing,
        completion: BootstrapCompletion,
    ) -> Result<()> {
        if self.bootstrap_complete {
            anyhow::ensure!(
                completion.watermarks.is_empty() && completion.state.is_none(),
                "completed bootstrap cannot accept another snapshot"
            );
        } else {
            anyhow::ensure!(
                self.bootstrap_configured,
                "transformer has no configured bootstrap"
            );
            let non_atomic = self.non_atomic;
            let observations = self.observations.clone();
            processing
                .run(move |context| async move {
                    anyhow::ensure!(
                        number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 0,
                        "transformer bootstrap already completed"
                    );
                    observations
                        .phase(QueryObservationKind::CommitWork, Phase::Bootstrap, async {
                            for watermark in completion.watermarks {
                                watermark.validate()?;
                                let key = match &watermark.source_id {
                                    Some(source) => SourceProgressKey::Source(source.clone()),
                                    None => SourceProgressKey::Stream(watermark.stream.clone()),
                                };
                                if let Some(producer) = &watermark.producer {
                                    context.bind_input_producer(producer, &key).await?;
                                }
                                context
                                    .stage_bootstrap_watermark(
                                        &key,
                                        watermark.sequence,
                                        watermark.position.as_deref(),
                                    )
                                    .await?;
                            }
                            if let Some(state) = completion.state {
                                context
                                    .stage_checkpoint(BOOTSTRAP_STATE_CHECKPOINT, 1, Some(&state))
                                    .await?;
                            }
                            context.stage_bootstrap_complete(true).await?;
                            if non_atomic
                                && context
                                    .state_info()
                                    .await?
                                    .non_atomic_reset
                                    .is_some_and(|marker| marker.in_progress)
                            {
                                context.finish_non_atomic_reset().await?;
                            }
                            Ok(())
                        })
                        .await
                })
                .await?;
            self.bootstrap_complete = true;
            self.refresh_schedule(processing).await?;
        }
        self.output.activate()
    }

    pub(super) fn stop(&mut self, failed: bool) -> Result<()> {
        self.rebuild = None;
        self.first_input_rebuild = false;
        if !self.volatile || failed {
            self.state = None;
        }
        if !self.volatile {
            self.pending = None;
        }
        self.replay = self.pending.is_some();
        self.schedule.publish(None)
    }

    pub(super) fn has_pending_emissions(&self) -> bool {
        self.replay
    }

    pub(super) fn resume_delivery(&mut self) {
        self.replay = self.pending.is_some();
    }

    pub(super) async fn transform(
        &mut self,
        processing: &NativeProcessing,
        input: InputEnvelope,
        input_scope: Option<drasi_computation_contracts::ComponentId>,
    ) -> Result<Option<ChangeEnvelope>> {
        anyhow::ensure!(
            self.pending.is_none(),
            "a transformer must complete pending output before another input"
        );
        anyhow::ensure!(
            input.envelope.system().control().is_none()
                || GraphChangeCodec::is_futures_due(&input.envelope)
                    && self.source_transactions.is_none(),
            "transformer received unsupported scheduled control"
        );
        let deadline = self.source_transactions.map(|limits| limits.duration());
        let complete_transaction = deadline.is_some();
        let expected = self.sequence;
        let volatile = self.volatile;
        let non_atomic = self.non_atomic;
        let output_state = self.output.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let logic = self.logic.clone();
        let state = self.opened()?;
        let observations = self.observations.clone();
        let counts_input =
            complete_transaction || !GraphChangeCodec::is_futures_due(&input.envelope);
        let first_live_input = self.unconnected_inputs && self.bootstrap_configured;
        if non_atomic
            && Box::pin(non_atomic::begin_input(
                processing,
                self.logic.clone(),
                input.clone(),
            ))
            .await?
        {
            return Ok(None);
        }
        let work = move |context: NativeProcessingContext| async move {
            let (context, _binding) = bound(&*logic, context)?;
            let prepared = observations
                .phase(QueryObservationKind::PrepareInput, Phase::Steady, async {
                    let sequence = context
                        .read_result_sequence()
                        .await?
                        .context("missing transformer result sequence")?;
                    anyhow::ensure!(
                        sequence == expected
                            && delivered_sequence(&context, volatile).await? == sequence,
                        "transformer state changed or output remains unconfirmed"
                    );
                    anyhow::ensure!(
                        number(&context, ROW_COUNT).await? == output_state.row_count()?,
                        "transformer view does not match its committed row count"
                    );
                    let progress = context.prepare_input_progress(&input).await?;
                    if first_live_input {
                        context
                            .stage_checkpoint(AWAITING_FIRST_INPUT, 0, None)
                            .await?;
                    }
                    if progress.duplicate {
                        return Ok(None);
                    }
                    let prepared = logic.prepare(
                        &input.envelope,
                        input_scope,
                        progress.source_id,
                        Phase::Steady,
                    )?;
                    Ok(Some((prepared, sequence)))
                })
                .await?;
            let Some((mut prepared, sequence)) = prepared else {
                return Ok((None, false));
            };
            let evaluation = logic
                .evaluate(&state, &mut prepared, Phase::Steady, &observations)
                .await?;
            let bytes = observations
                .phase(QueryObservationKind::PrepareOutput, Phase::Steady, async {
                    let next = sequence
                        .checked_add(1)
                        .context("transformer output sequence exhausted")?;
                    let output = logic.project(&input.envelope, prepared, evaluation, next)?;
                    context.stage_prepared_input_progress().await?;
                    Ok(match output {
                        Some(output) => Some(
                            stage_output(
                                &context,
                                &*logic,
                                &output_state,
                                output,
                                next,
                                retention,
                                volatile,
                                non_atomic,
                            )
                            .await?,
                        ),
                        None => None,
                    })
                })
                .await?;
            Ok((bytes, true))
        };
        let work = async {
            if non_atomic {
                Box::pin(processing.run_deferred_progress(work)).await
            } else {
                processing.run_output(work).await
            }
        };
        let (bytes, evaluated): (Option<Vec<u8>>, bool) = match deadline {
            Some(deadline) => tokio::time::timeout(deadline, Box::pin(work))
                .await
                .map_err(|_| drasi_computation_contracts::SourceTransactionError::Deadline)??,
            None => work.await?,
        };
        let core_return_ns = timestamp_ns();
        let bytes = if non_atomic {
            Box::pin(non_atomic::finish(
                processing,
                self.logic.clone(),
                self.output.clone(),
                retention,
                bytes,
            ))
            .await?
        } else {
            bytes
        };
        let finalization = self.observations.clone();
        self.unconnected_inputs = false;
        finalization
            .phase(QueryObservationKind::Finalize, Phase::Steady, async {
                let mut output = decode(bytes)?;
                if let Some(output) = &mut output {
                    self.logic.committed(output, core_return_ns)?;
                    self.sequence = logical_sequence(output)?;
                    self.pending = Some(output.clone());
                }
                if self.logic.take_schedule_change() {
                    self.refresh_schedule(processing).await?;
                }
                if evaluated && counts_input {
                    self.observations.input(Phase::Steady)?;
                }
                Ok(output)
            })
            .await
    }

    pub(super) async fn on_wakeup(
        &mut self,
        processing: &NativeProcessing,
        run: super::observations::Run,
    ) -> Result<(Option<ChangeEnvelope>, u64)> {
        anyhow::ensure!(
            self.bootstrap_complete && self.pending.is_none(),
            "a transformer must complete initialization and pending output before scheduled work"
        );
        let expected = self.sequence;
        let volatile = self.volatile;
        let logic = self.logic.clone();
        let state = self.opened()?;
        let output = self.output.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let non_atomic = self.non_atomic;
        let observations = self.observations.clone();
        if non_atomic {
            Box::pin(non_atomic::begin(processing)).await?;
        }
        let work = move |context: NativeProcessingContext| async move {
            let (context, _binding) = bound(&*logic, context)?;
            let now = context.physical_time().await?;
            anyhow::ensure!(
                context.read_result_sequence().await? == Some(expected)
                    && delivered_sequence(&context, volatile).await? == expected
                    && number(&context, ROW_COUNT).await? == output.row_count()?,
                "transformer state changed or output remains unconfirmed"
            );
            if logic
                .next_due(&state)
                .await?
                .is_none_or(|deadline| deadline > now)
            {
                return Ok((None, now));
            }
            run.start(QueryObservationKind::FutureQueue)?;
            run.scope(async {
                let next = expected
                    .checked_add(1)
                    .context("transformer output sequence exhausted")?;
                let bytes = match logic.due(&state, next, &observations).await? {
                    Some(due) => Some(
                        stage_output(
                            &context, &*logic, &output, due, next, retention, volatile, non_atomic,
                        )
                        .await?,
                    ),
                    None => None,
                };
                Ok((bytes, now))
            })
            .await
        };
        let (bytes, now): (Option<Vec<u8>>, u64) = if non_atomic {
            Box::pin(processing.run_deferred_progress(work)).await?
        } else {
            processing.run_output(work).await?
        };
        let bytes = if non_atomic {
            Box::pin(non_atomic::finish(
                processing,
                self.logic.clone(),
                self.output.clone(),
                retention,
                bytes,
            ))
            .await?
        } else {
            bytes
        };
        let output = decode(bytes)?;
        if let Some(output) = &output {
            self.sequence = logical_sequence(output)?;
            self.pending = Some(output.clone());
        }
        if self.logic.take_schedule_change() {
            self.refresh_schedule(processing).await?;
        }
        Ok((output, now))
    }

    pub(super) async fn continue_output(
        &mut self,
        processing: &NativeProcessing,
    ) -> Result<Option<ChangeEnvelope>> {
        if !self.replay {
            return Ok(None);
        }
        let pending = self
            .pending
            .as_ref()
            .context("missing pending transformer output")?;
        let sequence = self.sequence;
        let volatile = self.volatile;
        let emission: u64 = processing
            .run(move |context| async move {
                anyhow::ensure!(
                    context.read_result_sequence().await? == Some(sequence)
                        && delivered_sequence(&context, volatile).await?.checked_add(1)
                            == Some(sequence),
                    "transformer replay does not match retained progress"
                );
                let emission = context
                    .next_emission_sequence(number(&context, EMISSION).await?)
                    .await?;
                context.stage_checkpoint(EMISSION, emission, None).await?;
                Ok(emission)
            })
            .await?;
        let output = pending.reemit(emission)?;
        self.pending = Some(output.clone());
        self.replay = false;
        Ok(Some(output))
    }

    pub(super) async fn delivery_completed(
        &mut self,
        processing: &NativeProcessing,
        outputs: &[OutputEnvelope],
    ) -> Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        let [output] = outputs else {
            anyhow::bail!("a transformer confirmation must contain one output");
        };
        let logic = self.logic.clone();
        let sequence = self.output.validate_delivery(
            outputs,
            self.delivered,
            self.pending.as_ref(),
            &|envelope, producer, generation| logic.verify(envelope, producer, generation),
        )?;
        if sequence <= self.delivered {
            return Ok(());
        }
        anyhow::ensure!(
            self.pending.is_some(),
            "transformer has no output awaiting confirmation"
        );
        anyhow::ensure!(
            sequence == self.sequence,
            "transformer confirmation does not match pending output"
        );
        let emission = output.envelope.system().sequence();
        let volatile = self.volatile;
        let observations = self.observations.clone();
        processing
            .run(move |context| async move {
                anyhow::ensure!(
                    context.read_result_sequence().await? == Some(sequence)
                        && number(&context, EMISSION).await? == emission
                        && delivered_sequence(&context, volatile).await?.checked_add(1)
                            == Some(sequence),
                    "transformer confirmation would skip retained output"
                );
                observations
                    .phase(QueryObservationKind::CommitWork, Phase::Steady, async {
                        if volatile {
                            context
                                .stage_checkpoint(VOLATILE_DELIVERED, sequence, None)
                                .await?;
                        } else {
                            context.stage_delivery_progress(sequence).await?;
                        }
                        Ok(())
                    })
                    .await
            })
            .await?;
        self.delivered = sequence;
        self.pending = None;
        Ok(())
    }
}
