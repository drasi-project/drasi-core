// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use drasi_computation_contracts::profiling::timestamp_ns;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ChangeEnvelope, ComponentId, GraphChangeCodec,
    GraphProducerIdentity, InputEnvelope, OutputCursor, OutputEnvelope, QueryChangeCodec,
    QueryExecutionSettings, QueryRecoveryError, QueryRecoveryPolicy, ResultNodeProjector,
    SourceProgressKey, SourceTransactionLimits, StreamId, BOOTSTRAP_COMPLETE_CHECKPOINT,
    BOOTSTRAP_STATE_CHECKPOINT,
};
use drasi_computation_plugin_sdk::{CreateRequest, NativeProcessing, NativeProcessingContext};
use drasi_core::{
    computation::QueryInputBatch,
    interface::{FailureMode, OutboxPageLimits},
    query::QueryEvaluator,
};
use drasi_query_common::{ContinuousQueryDefinition, QueryOperation, QuerySchedule};

use super::{
    evaluator, indexed,
    output::{QueryOutput, Rows},
};

#[path = "indexed_non_atomic.rs"]
mod non_atomic;
#[path = "durable_recovery.rs"]
mod recovery;
#[path = "indexed_volatile.rs"]
mod volatile;

const CONFIGURATION: &str = "\0native-query:configuration:v1";
const UPDATE_OPERATION: &str = "\0native-query:update-operation:v1";
const PRODUCER: &str = "\0native-query:producer:v1";
const EMISSION: &str = "\0native-query:emission:v1";
const ROW_COUNT: &str = "\0native-query:live-row-count:v1";
const VOLATILE_DELIVERED: &str = "\0native-query:volatile-delivered:v1";

#[derive(serde::Serialize, serde::Deserialize)]
enum RecoveryAttempt {
    Ready(GraphProducerIdentity, u64, u64, bool),
    ResetRequired(String),
}

pub(super) struct DurableQuery {
    pub(super) volatile: bool,
    pub(super) non_atomic: bool,
    initialized: bool,
    configuration: Vec<u8>,
    producer: GraphProducerIdentity,
    sequence: u64,
    delivered: u64,
    pending: Option<ChangeEnvelope>,
    replay: bool,
    output: Arc<QueryOutput>,
    bootstrap_configured: bool,
    bootstrap_complete: bool,
    source_transactions: Option<SourceTransactionLimits>,
    recovery: QueryRecoveryPolicy,
    reset_configuration: bool,
    update_operation: Option<String>,
    indexes: Arc<indexed::Indexes>,
    evaluator: Option<Arc<QueryEvaluator>>,
    capacity: NonZeroUsize,
    schedule: QuerySchedule,
}

fn page_limits() -> OutboxPageLimits {
    OutboxPageLimits {
        max_records: NonZeroUsize::new(1).expect("one retained output"),
        max_bytes: NonZeroUsize::new(1 << 20).expect("positive page bound"),
    }
}

fn row_page_limits() -> OutboxPageLimits {
    OutboxPageLimits {
        max_records: NonZeroUsize::new(256).expect("positive row bound"),
        max_bytes: NonZeroUsize::new(1 << 20).expect("positive page bound"),
    }
}

async fn number(context: &NativeProcessingContext, key: &str) -> Result<u64> {
    let value = context
        .read_checkpoint(key)
        .await?
        .with_context(|| format!("missing query checkpoint {key:?}"))?;
    anyhow::ensure!(
        value.source_position.is_none(),
        "invalid query counter {key:?}"
    );
    Ok(value.sequence)
}

async fn delivered_sequence(context: &NativeProcessingContext, volatile: bool) -> Result<u64> {
    if volatile {
        number(context, VOLATILE_DELIVERED).await
    } else {
        context
            .read_delivery_progress()
            .await?
            .context("missing query delivery checkpoint")
    }
}

async fn stage_output(
    context: &NativeProcessingContext,
    state: &QueryOutput,
    projector: &ResultNodeProjector,
    mut output: ChangeEnvelope,
    retention: u64,
    volatile: bool,
    deferred: bool,
) -> Result<Vec<u8>> {
    let sequence = QueryChangeCodec::query_sequence(&output)?;
    if deferred {
        non_atomic::stage(
            context,
            drasi_query_common::QueryPublicationCheckpoint::Pending(sequence),
        )
        .await?;
        return Ok(
            drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?
                .encode(&output)?,
        );
    }
    state.stamp(&mut output)?;
    if !volatile {
        context
            .apply_live_rows(&QueryChangeCodec::live_row_mutations(projector, &output)?)
            .await?;
    }
    context
        .stage_checkpoint(ROW_COUNT, state.count_after(&output)?, None)
        .await?;
    let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
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

impl DurableQuery {
    pub(super) fn set_retention(&mut self, capacity: NonZeroUsize) {
        self.capacity = capacity;
    }

    pub(super) fn new(
        request: &CreateRequest,
        definition: &ContinuousQueryDefinition,
        output: Arc<QueryOutput>,
        execution: &QueryExecutionSettings,
        schedule: QuerySchedule,
    ) -> Result<Self> {
        let scope = request
            .scope
            .as_ref()
            .context("indexed query requires a host scope")?;
        let bootstrap_configured = request
            .configuration
            .get("bootstrap")
            .map(|value| value.as_bool().context("bootstrap must be a boolean"))
            .transpose()?
            .unwrap_or(false);
        let recovery = recovery::recovery_policy(request)?;
        let volatile = request
            .configuration
            .get("volatile")
            .map(|value| value.as_bool().context("volatile must be a boolean"))
            .transpose()?
            .unwrap_or(false);
        let non_atomic = non_atomic::configured(request)? && !volatile;
        anyhow::ensure!(
            !(volatile || non_atomic) || execution.source_transactions.is_none(),
            "complete source transactions require atomic publication"
        );
        anyhow::ensure!(
            recovery != QueryRecoveryPolicy::AutoReset || bootstrap_configured,
            "AutoReset requires configured provider bootstrap"
        );
        let mut configuration = serde_json::json!({
            "state_format": 8,
            "implementation": request.implementation,
            "configuration": serde_json::from_slice::<serde_json::Value>(&definition.configuration_bytes(execution)?)?,
            "bootstrap": bootstrap_configured,
            "instance": scope.instance_id,
            "graph": scope.graph_id,
            "component": request.id,
            "stream": definition.output_stream,
        });
        if non_atomic {
            configuration["publication"] = serde_json::json!("non_atomic");
        }
        let configuration = serde_json::to_vec(&configuration)?;
        Ok(Self {
            volatile,
            non_atomic,
            initialized: false,
            configuration,
            producer: GraphProducerIdentity::new(
                scope.instance_id.clone(),
                scope.graph_id.clone(),
                request.id.clone(),
                definition.output_stream.clone(),
                !volatile,
            )?,
            sequence: 0,
            delivered: 0,
            pending: None,
            replay: false,
            output,
            bootstrap_configured,
            bootstrap_complete: false,
            source_transactions: execution.source_transactions,
            recovery,
            reset_configuration: request
                .configuration
                .get("reset_configuration")
                .map(|value| {
                    value
                        .as_bool()
                        .context("reset_configuration must be a boolean")
                })
                .transpose()?
                .unwrap_or(false),
            update_operation: request
                .update_directive
                .as_ref()
                .map(|directive| directive.operation_id.clone()),
            indexes: Arc::new(indexed::Indexes::default()),
            evaluator: None,
            capacity: definition.outbox_capacity,
            schedule,
        })
    }

    pub(super) async fn start(
        &mut self,
        processing: &NativeProcessing,
        definition: Arc<ContinuousQueryDefinition>,
        execution: Arc<QueryExecutionSettings>,
        middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
        force_reset: bool,
    ) -> Result<()> {
        if self.volatile {
            return self
                .start_volatile(processing, definition, execution, middleware, force_reset)
                .await;
        }
        anyhow::ensure!(
            force_reset || self.evaluator.is_none(),
            "indexed evaluator is already initialized"
        );
        if self.non_atomic {
            Box::pin(self.prepare_non_atomic_reset(processing, force_reset)).await?;
        }
        let existing_evaluator = self.evaluator.take();
        let indexes = self.indexes.clone();
        let configuration = self.configuration.clone();
        let candidate = self.producer.clone();
        let restored = Arc::new(Mutex::new(None));
        let staged_rows = restored.clone();
        let output = self.output.clone();
        let bootstrap_configured = self.bootstrap_configured;
        let recovery = self.recovery;
        let reset_configuration = self.reset_configuration;
        let update_operation = self.update_operation.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let non_atomic = self.non_atomic;
        let retry = (definition.clone(), execution.clone(), middleware.clone());
        let attempt: RecoveryAttempt = processing
            .run(move |context| async move {
                let context = Arc::new(context);
                let _scope = indexes.bind(context.clone())?;
                let mut identified_read_only_state = false;
                let result: Result<(GraphProducerIdentity, u64, u64, bool)> = async {
                let profile = context.state_info().await?;
                anyhow::ensure!(
                    !profile.volatile && profile.non_atomic_publication == non_atomic
                        && profile.persistent_checkpoints,
                    "indexed query publication does not match its actual persistent profile"
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
                        force: force_reset,
                        reset_configuration,
                        update_operation: update_operation.as_deref(),
                    },
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
                            "invalid query bootstrap marker"
                        );
                        marker.sequence == 1
                    }
                    None if new_state => !bootstrap_configured,
                    None => anyhow::bail!("missing committed query bootstrap marker"),
                };
                anyhow::ensure!(
                    new_state || bootstrap_complete,
                    "incomplete durable bootstrap requires explicit recovery/reset"
                );
                let (producer, sequence, delivered) = if let Some(saved) = saved {
                    anyhow::ensure!(
                        saved.sequence == 1
                            && saved.source_position.as_deref() == Some(configuration.as_slice()),
                        "indexed query configuration changed; retained state cannot be reused"
                    );
                    let producer = recovery::producer(
                        producer.context("missing persisted query producer")?,
                        &candidate,
                    )?;
                    let sequence = sequence.context("missing query result sequence")?;
                    let delivered = delivered.context("missing query delivery checkpoint")?;
                    let emission = emission.context("missing query emission checkpoint")?;
                    anyhow::ensure!(
                        emission.source_position.is_none()
                            && delivered <= sequence
                            && sequence - delivered <= 1
                            && emission.sequence >= sequence,
                        "invalid query output/delivery progress"
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
                        "unidentified retained query state (sequence={sequence:?}, producer={}, delivered={}, emission={}, rows={}, bootstrap={}, resetting={})",
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
                let expected_rows = match count {
                    Some(count) => {
                        anyhow::ensure!(
                            count.source_position.is_none(),
                            QueryRecoveryError::Inconsistent("invalid stored query row count".into())
                        );
                        count.sequence
                    }
                    None if new_state => 0,
                    None => anyhow::bail!(QueryRecoveryError::Inconsistent(
                        "missing committed query row count".into()
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
                        let record = output.restore_row(signature, payload).with_context(|| {
                            QueryRecoveryError::Inconsistent(format!(
                                "failed to deserialize durable live row {signature}"
                            ))
                        })?;
                        anyhow::ensure!(
                            rows.insert(record.identity().clone(), record).is_none(),
                            QueryRecoveryError::Inconsistent("duplicate stored query row".into())
                        );
                        anyhow::ensure!(
                            u64::try_from(rows.len())? <= expected_rows,
                            QueryRecoveryError::Inconsistent("stored query rows exceed the committed row count".into())
                        );
                        after = Some(signature);
                    }
                }
                anyhow::ensure!(
                    u64::try_from(rows.len())? == expected_rows,
                    QueryRecoveryError::Inconsistent("stored query rows do not match committed row count".into())
                );
                let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
                let mut retained = Vec::new();
                if sequence == 0 || new_state {
                    anyhow::ensure!(
                        reset_plan.is_some()
                            || context.read_outbox_page(0, page_limits()).await?.is_empty(),
                        QueryRecoveryError::Inconsistent("query output exists without a committed sequence".into())
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
                                QueryRecoveryError::Inconsistent("query retained history is not contiguous".into())
                            );
                            let envelope = codec.decode(&bytes).with_context(|| {
                                QueryRecoveryError::Inconsistent(format!(
                                    "failed to deserialize durable outbox entry {stored_sequence}"
                                ))
                            })?;
                            anyhow::ensure!(
                                QueryChangeCodec::query_sequence(&envelope)? == stored_sequence,
                                QueryRecoveryError::Inconsistent("retained query output does not match committed progress".into())
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
                        "reset baseline disagrees with committed query state"
                    );
                    marker.sequence == sequence && retained.is_empty()
                } else {
                    false
                };
                drasi_query_common::validate_query_recovery(
                    &OutputCursor::try_new(producer.clone(), sequence)?,
                    &output.recovery_identity(&producer)?,
                    generation,
                    &rows,
                    &retained,
                    baseline,
                )?;
                let evaluator = if let Some(evaluator) = existing_evaluator {
                    anyhow::ensure!(
                        reset_plan.is_some(),
                        "retained evaluator requires an owned bootstrap reset"
                    );
                    // Reset Core's shadow queue with the same owned storage reset.
                    evaluator.future_queue().clear().await?;
                    evaluator
                } else {
                    Arc::new(evaluator(
                        &definition.query, definition.language, &execution, Some(indexes), middleware,
                    ).await?)
                };
                context.recover_input_progress().await?;
                *staged_rows
                    .lock()
                    .map_err(|_| anyhow::anyhow!("query hydration state poisoned"))? =
                    Some((rows, evaluator, retained, generation));
                Ok((producer, sequence, delivered, bootstrap_complete))
                }.await;
                match result {
                    Ok((producer, sequence, delivered, complete)) =>
                        Ok(RecoveryAttempt::Ready(producer, sequence, delivered, complete)),
                    Err(error) if !force_reset && identified_read_only_state && bootstrap_configured
                        && drasi_query_common::should_reset_query_error(&error, recovery, reset_configuration) =>
                        Ok(RecoveryAttempt::ResetRequired(format!("{error:#}"))),
                    Err(error) => Err(error),
                }
            })
            .await?;
        let (producer, sequence, delivered, bootstrap_complete) = match attempt {
            RecoveryAttempt::Ready(producer, sequence, delivered, complete) => {
                (producer, sequence, delivered, complete)
            }
            RecoveryAttempt::ResetRequired(reason) => {
                log::warn!(
                    "Rebuilding query '{}' under AutoReset after retained-state validation failed: {reason}",
                    retry.0.id,
                );
                return Box::pin(self.start(processing, retry.0, retry.1, retry.2, true)).await;
            }
        };
        let (rows, evaluator, retained, generation) = restored
            .lock()
            .map_err(|_| anyhow::anyhow!("query hydration state poisoned"))?
            .take()
            .context("query hydration did not complete")?;
        self.pending =
            drasi_query_common::pending_query_outputs(&retained, sequence, delivered)?.pop_front();
        self.replay = self.pending.is_some();
        self.producer = producer;
        self.sequence = sequence;
        self.delivered = delivered;
        self.bootstrap_complete = bootstrap_complete;
        self.output.restore(
            self.producer.clone(),
            self.sequence,
            rows,
            false,
            retained,
            generation,
        )?;
        self.evaluator = Some(evaluator);
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
                self.recovery == QueryRecoveryPolicy::AutoReset && self.bootstrap_configured,
                "failed volatile query requires explicit reconstruction and bootstrap"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn refresh_schedule(&self, processing: &NativeProcessing) -> Result<()> {
        let indexes = self.indexes.clone();
        let evaluator = self
            .evaluator
            .clone()
            .context("indexed evaluator is not initialized")?;
        let next_due = processing
            .run(move |context| async move {
                let _scope = indexes.bind(Arc::new(context))?;
                Ok(evaluator.future_queue().peek_due_time().await?)
            })
            .await?;
        self.indexes.take_future_changes();
        self.schedule.publish(next_due)
    }

    pub(super) fn bootstrap_status(&self) -> BootstrapStatus {
        BootstrapStatus {
            configured: self.bootstrap_configured,
            complete: self.bootstrap_complete,
            reset_allowed: self.recovery == QueryRecoveryPolicy::AutoReset,
        }
    }

    pub(super) async fn bootstrap(
        &self,
        processing: &NativeProcessing,
        definition: Arc<ContinuousQueryDefinition>,
        projector: ResultNodeProjector,
        input: InputEnvelope,
        input_scope: Option<ComponentId>,
    ) -> Result<()> {
        anyhow::ensure!(
            self.bootstrap_configured && !self.bootstrap_complete,
            "indexed query is not accepting initialization"
        );
        let output = self.output.clone();
        let volatile = self.volatile;
        let non_atomic = self.non_atomic;
        let sequence = self.sequence;
        let bootstrap_projection = non_atomic.then(|| (output.clone(), projector.clone()));
        let indexes = self.indexes.clone();
        let evaluator = self
            .evaluator
            .clone()
            .context("indexed evaluator is not initialized")?;
        let bytes: Option<Vec<u8>> = processing
            .run(move |context| async move {
                let context = Arc::new(context);
                let _scope = indexes.bind(context.clone())?;
                anyhow::ensure!(
                    number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 0
                        && context.read_result_sequence().await? == Some(sequence),
                    "bootstrap cannot replace a live output generation"
                );
                let operation = QueryOperation::new(&input.envelope)?;
                let results = evaluator
                    .evaluate_input_batch::<false>(
                        QueryInputBatch::new(
                            GraphChangeCodec::decode_changes(&input.envelope)?,
                            input.envelope.system().ordering(),
                        )
                        .with_optional_input_scope(input_scope),
                    )
                    .await?;
                let Some(envelope) =
                    operation.project(&definition, &projector, sequence, &results)?
                else {
                    return Ok(None);
                };
                anyhow::ensure!(
                    number(&context, ROW_COUNT).await? == output.row_count()?,
                    "bootstrap projection does not match committed rows"
                );
                if !volatile && !non_atomic {
                    context
                        .apply_live_rows(&QueryChangeCodec::live_row_mutations(
                            &projector, &envelope,
                        )?)
                        .await?;
                }
                if !non_atomic {
                    context
                        .stage_checkpoint(ROW_COUNT, output.count_after(&envelope)?, None)
                        .await?;
                }
                Ok(Some(
                    drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?
                        .encode(&envelope)?,
                ))
            })
            .await?;
        if let Some(bytes) = bytes {
            let envelope =
                drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?
                    .decode(&bytes)?;
            if let Some((state, projector)) = bootstrap_projection {
                let projection = envelope.clone();
                processing
                    .run(move |context| async move {
                        anyhow::ensure!(
                            number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 0
                                && context.read_result_sequence().await? == Some(sequence)
                                && number(&context, ROW_COUNT).await? == state.row_count()?,
                            "bootstrap projection changed before non-atomic publication"
                        );
                        context
                            .apply_live_rows(&QueryChangeCodec::live_row_mutations(
                                &projector,
                                &projection,
                            )?)
                            .await?;
                        context
                            .stage_checkpoint(ROW_COUNT, state.count_after(&projection)?, None)
                            .await?;
                        Ok(())
                    })
                    .await?;
            }
            self.output.apply_bootstrap(&envelope)?;
        }
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
                "query has no configured bootstrap"
            );
            let non_atomic = self.non_atomic;
            processing
                .run(move |context| async move {
                    anyhow::ensure!(
                        number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 0,
                        "query bootstrap already completed"
                    );
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
                .await?;
            self.bootstrap_complete = true;
            self.refresh_schedule(processing).await?;
        }
        self.output.activate()
    }

    pub(super) fn stop(&mut self, failed: bool) -> Result<()> {
        if !self.volatile || failed {
            self.evaluator = None;
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
        definition: Arc<ContinuousQueryDefinition>,
        projector: ResultNodeProjector,
        input: InputEnvelope,
        input_scope: Option<ComponentId>,
    ) -> Result<Option<ChangeEnvelope>> {
        anyhow::ensure!(
            self.pending.is_none(),
            "query must complete pending output before another input"
        );
        anyhow::ensure!(
            input.envelope.system().control().is_none()
                || GraphChangeCodec::is_futures_due(&input.envelope)
                    && self.source_transactions.is_none(),
            "query received unsupported scheduled control"
        );
        let prepared_input = match self.source_transactions {
            Some(_) => drasi_query_common::prepare_query_input::<true>(
                &input.envelope,
                self.source_transactions,
            )?,
            None => drasi_query_common::prepare_query_input::<false>(&input.envelope, None)?,
        };
        let deadline = self.source_transactions.map(|limits| limits.duration());
        let complete_transaction = deadline.is_some();
        let expected = self.sequence;
        let volatile = self.volatile;
        let non_atomic = self.non_atomic;
        let completion = non_atomic.then(|| (self.output.clone(), projector.clone()));
        let output_state = self.output.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let indexes = self.indexes.clone();
        let evaluator = self
            .evaluator
            .clone()
            .context("indexed evaluator is not initialized")?;
        let component = definition.id.clone();
        if non_atomic && Box::pin(non_atomic::begin_input(processing, input.clone())).await? {
            return Ok(None);
        }
        let work = move |context: NativeProcessingContext| async move {
            let context = Arc::new(context);
            let _scope = indexes.bind(context.clone())?;
            let sequence = context
                .read_result_sequence()
                .await?
                .context("missing query result sequence")?;
            anyhow::ensure!(
                sequence == expected && delivered_sequence(&context, volatile).await? == sequence,
                "query state changed or output remains unconfirmed"
            );
            anyhow::ensure!(
                number(&context, ROW_COUNT).await? == output_state.row_count()?,
                "query snapshot does not match committed row count"
            );
            let progress = context.prepare_input_progress(&input).await?;
            if progress.duplicate {
                return Ok(None);
            }
            let mut operation = QueryOperation::new(&input.envelope)?;
            if let Some(source) = prepared_input.source_id.clone().or(progress.source_id) {
                operation = operation.with_source_id(source);
            }
            let changes = prepared_input.into_changes(&input.envelope)?;
            let batch = QueryInputBatch::new(changes, input.envelope.system().ordering())
                .with_optional_input_scope(input_scope);
            let results = if complete_transaction {
                evaluator.evaluate_input_batch::<true>(batch).await?
            } else {
                evaluator.evaluate_input_batch::<false>(batch).await?
            };
            let next = QueryOperation::output_sequence(sequence, &results)?;
            let output = operation.project(&definition, &projector, next, &results)?;
            context.stage_prepared_input_progress().await?;
            let bytes = match output {
                Some(output) => Some(
                    stage_output(
                        &context,
                        &output_state,
                        &projector,
                        output,
                        retention,
                        volatile,
                        non_atomic,
                    )
                    .await?,
                ),
                None => None,
            };
            Ok(bytes)
        };
        let work = async {
            if non_atomic {
                Box::pin(processing.run_deferred_progress(work)).await
            } else {
                processing.run_output(work).await
            }
        };
        let bytes: Option<Vec<u8>> = match deadline {
            Some(deadline) => tokio::time::timeout(deadline, Box::pin(work))
                .await
                .map_err(|_| drasi_computation_contracts::SourceTransactionError::Deadline)??,
            None => work.await?,
        };
        let core_return_ns = timestamp_ns();
        let bytes = match completion {
            Some((state, projector)) => {
                Box::pin(non_atomic::finish(
                    processing, state, projector, retention, bytes,
                ))
                .await?
            }
            None => bytes,
        };
        let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
        let mut output = bytes.map(|bytes| codec.decode(&bytes)).transpose()?;
        if let Some(output) = &mut output {
            QueryOperation::committed(output, &component, core_return_ns)?;
        }
        if let Some(output) = &output {
            self.sequence = QueryChangeCodec::query_sequence(output)?;
            self.pending = Some(output.clone());
        }
        if self.indexes.take_future_changes() {
            self.refresh_schedule(processing).await?;
        }
        Ok(output)
    }

    pub(super) async fn on_wakeup(
        &mut self,
        processing: &NativeProcessing,
        definition: Arc<ContinuousQueryDefinition>,
        projector: ResultNodeProjector,
    ) -> Result<(Option<ChangeEnvelope>, u64)> {
        anyhow::ensure!(
            self.bootstrap_complete && self.pending.is_none(),
            "query must complete initialization and pending output before scheduled work"
        );
        let expected = self.sequence;
        let volatile = self.volatile;
        let indexes = self.indexes.clone();
        let evaluator = self
            .evaluator
            .clone()
            .context("indexed evaluator is not initialized")?;
        let state = self.output.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let non_atomic = self.non_atomic;
        let completion = non_atomic.then(|| (state.clone(), projector.clone()));
        if non_atomic {
            Box::pin(non_atomic::begin(processing)).await?;
        }
        let work = move |context: NativeProcessingContext| async move {
            let context = Arc::new(context);
            let _scope = indexes.bind(context.clone())?;
            let now = context.physical_time().await?;
            anyhow::ensure!(
                context.read_result_sequence().await? == Some(expected)
                    && delivered_sequence(&context, volatile).await? == expected
                    && number(&context, ROW_COUNT).await? == state.row_count()?,
                "query state changed or output remains unconfirmed"
            );
            let next_due = evaluator.future_queue().peek_due_time().await?;
            if next_due.is_none_or(|deadline| deadline > now) {
                return Ok((None, now));
            }
            let output = match evaluator.evaluate_next_future().await? {
                Some(due) => {
                    let sequence = QueryOperation::output_sequence(expected, &due.results)?;
                    drasi_query_common::project_due_query_output(
                        &definition,
                        &projector,
                        sequence,
                        &due,
                    )?
                }
                None => None,
            };
            let bytes = match output {
                Some(output) => Some(
                    stage_output(
                        &context, &state, &projector, output, retention, volatile, non_atomic,
                    )
                    .await?,
                ),
                None => None,
            };
            Ok((bytes, now))
        };
        let (bytes, now): (Option<Vec<u8>>, u64) = if non_atomic {
            Box::pin(processing.run_deferred_progress(work)).await?
        } else {
            processing.run_output(work).await?
        };
        let bytes = match completion {
            Some((state, projector)) => {
                Box::pin(non_atomic::finish(
                    processing, state, projector, retention, bytes,
                ))
                .await?
            }
            None => bytes,
        };
        let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
        let output = bytes.map(|bytes| codec.decode(&bytes)).transpose()?;
        if let Some(output) = &output {
            self.sequence = QueryChangeCodec::query_sequence(output)?;
            self.pending = Some(output.clone());
        }
        if self.indexes.take_future_changes() {
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
            .context("missing pending query output")?;
        let sequence = self.sequence;
        let volatile = self.volatile;
        let emission: u64 = processing
            .run(move |context| async move {
                anyhow::ensure!(
                    context.read_result_sequence().await? == Some(sequence)
                        && delivered_sequence(&context, volatile).await?.checked_add(1)
                            == Some(sequence),
                    "query replay does not match retained progress"
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
            anyhow::bail!("query confirmation must contain one output");
        };
        let sequence =
            self.output
                .validate_delivery(outputs, self.delivered, self.pending.as_ref())?;
        if sequence <= self.delivered {
            return Ok(());
        }
        anyhow::ensure!(
            self.pending.is_some(),
            "query has no output awaiting confirmation"
        );
        anyhow::ensure!(
            sequence == self.sequence,
            "query confirmation does not match pending output"
        );
        let emission = output.envelope.system().sequence();
        let volatile = self.volatile;
        processing
            .run(move |context| async move {
                anyhow::ensure!(
                    context.read_result_sequence().await? == Some(sequence)
                        && number(&context, EMISSION).await? == emission
                        && delivered_sequence(&context, volatile).await?.checked_add(1)
                            == Some(sequence),
                    "query confirmation would skip retained output"
                );
                if volatile {
                    context
                        .stage_checkpoint(VOLATILE_DELIVERED, sequence, None)
                        .await?;
                } else {
                    context.stage_delivery_progress(sequence).await?;
                }
                Ok(())
            })
            .await?;
        self.delivered = sequence;
        self.pending = None;
        Ok(())
    }
}
