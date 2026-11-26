// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The continuous query's logic: evaluate input changes against the query and
//! publish the resulting changes to its result rows. Storage, bootstrap,
//! recovery and the output are the shared stateful-transformer runtime.

use std::{
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::telemetry::{QueryObservationKind, QueryObservationPhase};
use drasi_computation_contracts::{
    profiling::ProfilingMetadata, ChangeEnvelope, ChangeOperation, ComponentId,
    ComponentUpdatePlan, GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress,
    InputBinding, InputEnvelope, OutputReaders, PortId, QueryChangeCodec, QueryDescription,
    QueryExecutionSettings, QueryInputOrigin, QueryOutputMetadata, QueryRecoveryError,
    QueryRecoveryIdentity, Record, ResultNodeProjector, StreamId, SystemMetadata,
};
use drasi_computation_plugin_sdk::{
    transformer::{Observations, Phase, Rows, TransformerLogic, TransformerOutput, OUTPUT_PORT},
    ApplyUpdateRequest, FactoryMetadata, NativeProcessingContext, UpdateRequest,
};
use drasi_core::{
    computation::QueryInputBatch, evaluation::context::QueryPartEvaluationContext,
    middleware::MiddlewareTypeRegistry, query::QueryEvaluator,
};
use drasi_query_common::{ContinuousQueryDefinition, QueryInputScopes, QueryOperation};

use super::{evaluator, indexed, observations, update, view::QueryView};

pub(super) struct QueryLogic {
    pub(super) definition: Arc<ContinuousQueryDefinition>,
    pub(super) execution: Arc<QueryExecutionSettings>,
    pub(super) middleware: Arc<MiddlewareTypeRegistry>,
    pub(super) projector: ResultNodeProjector,
    pub(super) indexes: Arc<indexed::Indexes>,
    pub(super) input_scopes: RwLock<Option<QueryInputScopes>>,
    pub(super) description: QueryDescription,
    pub(super) metadata: FactoryMetadata,
}

/// One input batch and the provenance its output reports.
pub(super) struct Prepared {
    origin: QueryInputOrigin,
    profiling: ProfilingMetadata,
    batch: Option<QueryInputBatch>,
    transactional: bool,
}

impl QueryLogic {
    fn scopes(&self) -> Result<std::sync::RwLockReadGuard<'_, Option<QueryInputScopes>>> {
        self.input_scopes
            .read()
            .map_err(|_| anyhow::anyhow!("query input scopes poisoned"))
    }

    fn identity(&self, producer: &GraphProducerIdentity) -> Result<QueryRecoveryIdentity> {
        Ok(QueryRecoveryIdentity::for_producer(
            producer,
            self.description.configuration_hash,
        )?)
    }
}

fn inconsistent(message: &str) -> QueryRecoveryError {
    QueryRecoveryError::Inconsistent(message.into())
}

#[async_trait]
impl TransformerLogic for QueryLogic {
    type State = QueryEvaluator;
    type Prepared = Prepared;
    type Evaluation = Vec<QueryPartEvaluationContext>;

    fn bind(&self, context: &Arc<NativeProcessingContext>) -> Result<Box<dyn Send>> {
        Ok(Box::new(self.indexes.bind(context.clone())?))
    }

    async fn open(&self, _context: &Arc<NativeProcessingContext>) -> Result<Arc<QueryEvaluator>> {
        Ok(Arc::new(
            evaluator(
                &self.definition.query,
                self.definition.language,
                &self.execution,
                Some(self.indexes.clone()),
                self.middleware.clone(),
            )
            .await?,
        ))
    }

    async fn reset(&self, evaluator: &QueryEvaluator) -> Result<()> {
        // Core's shadow queue is reset with the same owned storage reset.
        Ok(evaluator.future_queue().clear().await?)
    }

    fn scope(&self, input: &InputEnvelope, phase: Phase) -> Result<Option<ComponentId>> {
        let scopes = self.scopes()?;
        if phase == Phase::Bootstrap {
            return Ok(scopes.as_ref().and_then(|scopes| {
                scopes
                    .get(&(input.port.clone(), input.envelope.system().stream().clone()))
                    .cloned()
            }));
        }
        drasi_query_common::query_input_scope(scopes.as_ref(), input)
    }

    fn bind_inputs(&self, bindings: &[InputBinding]) -> Result<()> {
        let scopes = if self
            .execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
        {
            Some(drasi_query_common::query_input_scopes(
                &self.definition.id,
                &self.execution.inputs,
                bindings,
            )?)
        } else {
            None
        };
        *self
            .input_scopes
            .write()
            .map_err(|_| anyhow::anyhow!("query input scopes poisoned"))? = scopes;
        Ok(())
    }

    fn prepare(
        &self,
        input: &ChangeEnvelope,
        scope: Option<ComponentId>,
        source: Option<ComponentId>,
        phase: Phase,
    ) -> Result<Prepared> {
        if phase == Phase::Bootstrap {
            let (origin, profiling) = QueryOperation::new(input)?.into_parts();
            return Ok(Prepared {
                origin,
                profiling,
                batch: Some(
                    QueryInputBatch::new(
                        GraphChangeCodec::decode_changes(input)?,
                        input.system().ordering(),
                    )
                    .with_optional_input_scope(scope),
                ),
                transactional: false,
            });
        }
        let limits = self.execution.source_transactions;
        let prepared = match limits {
            Some(_) => drasi_query_common::prepare_query_input::<true>(input, limits)?,
            None => drasi_query_common::prepare_query_input::<false>(input, None)?,
        };
        let mut operation = QueryOperation::new(input)?;
        if let Some(source) = prepared.source_id.clone().or(source) {
            operation = operation.with_source_id(source);
        }
        let changes = prepared.into_changes(input)?;
        let (origin, profiling) = operation.into_parts();
        Ok(Prepared {
            origin,
            profiling,
            batch: Some(
                QueryInputBatch::new(changes, input.system().ordering())
                    .with_optional_input_scope(scope),
            ),
            transactional: limits.is_some(),
        })
    }

    async fn evaluate(
        &self,
        evaluator: &QueryEvaluator,
        prepared: &mut Prepared,
        phase: Phase,
        observations: &Observations,
    ) -> Result<Vec<QueryPartEvaluationContext>> {
        let batch = prepared.batch.take().context("query batch was evaluated")?;
        Ok(if prepared.transactional {
            observations::evaluate::<true>(observations, evaluator, batch, phase).await?
        } else {
            observations::evaluate::<false>(observations, evaluator, batch, phase).await?
        })
    }

    fn project(
        &self,
        input: &ChangeEnvelope,
        prepared: Prepared,
        results: Vec<QueryPartEvaluationContext>,
        sequence: u64,
    ) -> Result<Option<ChangeEnvelope>> {
        Ok(
            QueryOperation::from_parts(input, prepared.origin, prepared.profiling).project(
                &self.definition,
                &self.projector,
                sequence,
                &results,
            )?,
        )
    }

    fn committed(&self, output: &mut ChangeEnvelope, core_return_ns: u64) -> Result<()> {
        QueryOperation::committed(output, &self.definition.id, core_return_ns)
    }

    fn take_schedule_change(&self) -> bool {
        self.indexes.take_future_changes()
    }

    async fn next_due(&self, evaluator: &QueryEvaluator) -> Result<Option<u64>> {
        Ok(evaluator.future_queue().peek_due_time().await?)
    }

    async fn due(
        &self,
        evaluator: &QueryEvaluator,
        sequence: u64,
        observations: &Observations,
    ) -> Result<Option<ChangeEnvelope>> {
        let Some(due) = observations::future(observations, evaluator).await? else {
            return Ok(None);
        };
        observations
            .phase(
                QueryObservationKind::PrepareOutput,
                QueryObservationPhase::Steady,
                async {
                    Ok(drasi_query_common::project_due_query_output(
                        &self.definition,
                        &self.projector,
                        sequence,
                        &due,
                    )?)
                },
            )
            .await
    }

    fn decorate(
        &self,
        output: &mut ChangeEnvelope,
        producer: &GraphProducerIdentity,
        generation: u64,
    ) -> Result<()> {
        let identity = self.identity(producer)?;
        QueryChangeCodec::set_generation(output, &self.definition.id, generation)?;
        identity.annotate(output, &self.definition.id)?;
        Ok(())
    }

    fn verify(
        &self,
        output: &ChangeEnvelope,
        producer: &GraphProducerIdentity,
        generation: u64,
    ) -> Result<()> {
        let metadata = QueryChangeCodec::metadata(output)
            .map_err(anyhow::Error::from)
            .context(inconsistent("invalid query output metadata"))?;
        anyhow::ensure!(
            metadata.query_id == self.definition.id,
            QueryRecoveryError::OwnershipMismatch
        );
        anyhow::ensure!(
            QueryChangeCodec::query_generation(output)
                .map_err(anyhow::Error::from)
                .context(inconsistent("invalid query output generation"))?
                == generation,
            inconsistent("query output belongs to another reset generation")
        );
        match QueryRecoveryIdentity::optional_from_envelope(output)
            .map_err(anyhow::Error::from)
            .context(inconsistent("invalid query output identity"))?
        {
            Some(stored) if stored == self.identity(producer)? => {}
            Some(_) => anyhow::bail!(QueryRecoveryError::OwnershipMismatch),
            None => anyhow::bail!(inconsistent("query output has no producer identity")),
        }
        let progress = GraphProducerProgress::from_envelope(output)?
            .context(inconsistent("query output has no producer progress"))?;
        anyhow::ensure!(
            QueryChangeCodec::query_sequence(output)? == progress.sequence(),
            inconsistent("query output position differs from its producer progress")
        );
        Ok(())
    }

    fn verify_owner(
        &self,
        output: &ChangeEnvelope,
        producer: &GraphProducerIdentity,
    ) -> Result<()> {
        match QueryRecoveryIdentity::optional_from_envelope(output) {
            Ok(Some(stored)) => drasi_query_common::validate_query_reset_owner(
                &QueryRecoveryIdentity::for_producer(producer, stored.configuration_hash())?,
                Some(&stored),
            ),
            Ok(None) => Ok(()),
            Err(error) => {
                log::warn!(
                    "Authorized reset of query '{}' encountered an undecodable output identity: {error:#}",
                    self.definition.id,
                );
                Ok(())
            }
        }
    }

    fn row_mutations(
        &self,
        output: &ChangeEnvelope,
        _after: &Rows,
    ) -> Result<Vec<(u64, Option<Bytes>)>> {
        // Query rows are stored under the evaluator's own row signatures.
        Ok(
            QueryChangeCodec::live_row_mutations(&self.projector, output)?
                .into_iter()
                .zip(output.changes().operations())
                .map(|(mutation, operation)| {
                    (
                        mutation.row_signature,
                        match operation {
                            ChangeOperation::Added { after, .. }
                            | ChangeOperation::Updated { after, .. } => {
                                Some(after.payload().clone())
                            }
                            ChangeOperation::Deleted { .. } => None,
                        },
                    )
                })
                .collect(),
        )
    }

    fn restore_row(&self, key: u64, payload: Bytes) -> Result<Record> {
        Ok(QueryChangeCodec::restore_live_row(
            &self.projector,
            key,
            payload,
        )?)
    }

    fn difference(
        &self,
        old: &Rows,
        new: &Rows,
        stream: &StreamId,
        sequence: u64,
        upstream: &StreamId,
    ) -> Result<Option<ChangeEnvelope>> {
        let delta = difference(&self.projector, old, new)?;
        Ok(QueryChangeCodec::encode_evaluation(
            None,
            &self.definition.id,
            SystemMetadata::new(stream.clone(), sequence),
            &self.projector,
            &delta,
            QueryOutputMetadata {
                query_id: self.definition.id.clone(),
                input_origin: Some(QueryInputOrigin::Stream(upstream.clone())),
                timestamp: chrono::Utc::now(),
                metadata: Default::default(),
                profiling: None,
            },
        )?)
    }

    fn rebuild_supported(&self) -> bool {
        // Input middleware pipelines are not replayed by a rebuild.
        self.execution.middleware.is_empty()
    }

    fn readers(&self, output: &Arc<TransformerOutput>, readers: OutputReaders) -> OutputReaders {
        readers.with_query(
            PortId::try_new(OUTPUT_PORT).expect("constant port"),
            Arc::new(QueryView::new(
                output.clone(),
                self.projector.clone(),
                self.description.clone(),
            )),
        )
    }

    fn manages_updates(&self) -> bool {
        true
    }

    fn plan_update(
        &self,
        _state: Option<&Self::State>,
        request: &UpdateRequest,
    ) -> Option<Result<ComponentUpdatePlan>> {
        Some(update::plan(request, &self.middleware, &self.metadata))
    }

    fn apply_update(&self, request: &ApplyUpdateRequest) -> Result<NonZeroUsize> {
        let (definition, _) =
            super::query_configuration(&request.request.proposed, &self.middleware)?;
        Ok(definition.outbox_capacity)
    }
}

/// The result rows that move a dependent from `old` to `new`, compared by
/// value rather than by the effective time a re-evaluation stamps on a row.
fn difference(
    projector: &ResultNodeProjector,
    old: &Rows,
    new: &Rows,
) -> Result<Vec<QueryPartEvaluationContext>> {
    let mut keys: Vec<_> = old.keys().chain(new.keys()).collect();
    keys.sort_unstable_by(|a, b| (a.namespace(), a.value()).cmp(&(b.namespace(), b.value())));
    keys.dedup();
    let mut result = Vec::new();
    for key in keys {
        let decode = |row| QueryChangeCodec::decode_row_in(projector, row);
        let before = old.get(key).map(decode).transpose()?;
        let after = new.get(key).map(decode).transpose()?;
        let signature = after
            .as_ref()
            .or(before.as_ref())
            .and_then(|row| row.signature)
            .context("replacement result is not keyed by a query row signature")?;
        use QueryPartEvaluationContext::*;
        result.push(match (before, after) {
            (None, Some(after)) => Adding {
                after: after.values,
                row_signature: signature,
            },
            (Some(before), None) => Removing {
                before: before.values,
                row_signature: signature,
            },
            (Some(before), Some(after)) => {
                if GraphChangeCodec::decode_element(&old[key])?.properties()
                    == GraphChangeCodec::decode_element(&new[key])?.properties()
                {
                    continue;
                }
                Updating {
                    before: before.values,
                    after: after.values,
                    row_signature: signature,
                }
            }
            (None, None) => unreachable!("key came from one of the two views"),
        });
    }
    Ok(result)
}
