// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use crate::{
    config::{AssignedQueryConfig, ResolvedQueryConfig as QueryConfig},
    metrics::QueryOutputMetrics,
    queries::{FetchError, OutboxResponse, SnapshotResponse},
};

use super::{query_catalog::LocalQueryOutput, QueryResultsCatalog};
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, OutputCursor, OutputReadError, QueryOutputObservation,
    QueryOutputView, QueryResultSnapshot, QueryViewProvider, ResultNodeProjector,
};
use drasi_query_common::QueryExecutionSettingsExt;
#[cfg(test)]
#[path = "query_api_tests.rs"]
mod tests;

pub(super) enum QueryApiOutput {
    Local(LocalQueryOutput),
    Native {
        reader: Arc<dyn QueryViewProvider>,
        progress: Option<Arc<super::QuerySourceProgress>>,
    },
}

#[derive(Clone)]
pub(super) struct NativeQueryProgress {
    input: Option<Arc<super::SourceProgressSnapshot>>,
    cursor: OutputCursor,
    generation: u64,
}

/// Read access to the results of an existing query. It does not own an evaluator
/// or perform lifecycle operations; the containing component remains the owner.
pub struct QueryApi {
    pub(crate) config: QueryConfig,
    pub(crate) catalog: Option<QueryResultsCatalog>,
    pub(crate) metrics: Arc<QueryOutputMetrics>,
    pub(super) native_progress: Arc<std::sync::Mutex<Option<NativeQueryProgress>>>,
    pub(super) output: Arc<QueryApiOutput>,
    pub(super) _registration: Option<super::query_catalog::QueryRegistration>,
    pub(super) links:
        std::sync::Mutex<Vec<(QueryResultsCatalog, super::query_catalog::CatalogLink)>>,
}

impl QueryApi {
    pub(crate) fn native_activation_recorded(&self) -> anyhow::Result<bool> {
        Ok(self
            .native_progress
            .lock()
            .map_err(|_| anyhow::anyhow!("native query metrics poisoned"))?
            .is_some())
    }

    pub fn recovery_reader(
        &self,
    ) -> Option<Arc<dyn drasi_computation_contracts::QueryRecoveryReader>> {
        match self.output.as_ref() {
            QueryApiOutput::Local(output) => Some(Arc::new(output.results.clone())),
            QueryApiOutput::Native { reader, .. } => reader.clone().recovery_reader(),
        }
    }

    #[must_use = "retain the measurement until the native operation finishes"]
    pub fn native_operation_timer(&self) -> impl Send + 'static {
        crate::metrics::query_metrics::QueryOperationTimer {
            metrics: Some(self.metrics.clone()),
            started: std::time::Instant::now(),
        }
    }

    pub(crate) fn output_metrics(&self) -> Option<Arc<QueryOutputMetrics>> {
        match self.output.as_ref() {
            QueryApiOutput::Native { reader, .. } if !reader.supports_observations() => None,
            _ => Some(self.metrics.clone()),
        }
    }

    fn record_native_observation(
        &self,
        observation: &QueryOutputObservation,
    ) -> anyhow::Result<()> {
        let QueryApiOutput::Native { reader, .. } = self.output.as_ref() else {
            anyhow::bail!("native observation requires a native query");
        };
        observation.validate(&self.config.id, &reader.description())?;
        self.metrics.update_outbox(
            observation.outbox_size,
            observation.oldest_sequence.unwrap_or(0),
            observation.cursor.sequence(),
        );
        self.metrics
            .record_live_results_count(observation.live_results);
        Ok(())
    }

    /// Capture the actual initial frontier before the host admits query input.
    /// This retains progress metadata only, never another result projection.
    pub async fn initialize_native_progress(&self) -> anyhow::Result<()> {
        let QueryApiOutput::Native {
            reader, progress, ..
        } = self.output.as_ref()
        else {
            return Ok(());
        };
        let (head, generation) = if reader.supports_observations() {
            let observation = reader.query_observation().await?;
            self.record_native_observation(&observation)?;
            (observation.cursor, observation.generation)
        } else {
            reader.query_head().await?
        };
        *self
            .native_progress
            .lock()
            .map_err(|_| anyhow::anyhow!("native query metrics poisoned"))? =
            Some(NativeQueryProgress {
                input: progress.as_ref().map(|progress| progress.snapshot()),
                cursor: head,
                generation,
            });
        Ok(())
    }

    /// The host calls this only after the native operation has completed.
    pub fn observe_native_progress(
        &self,
        outputs: &[super::OutputEnvelope],
        observation: Option<&QueryOutputObservation>,
    ) -> anyhow::Result<()> {
        let QueryApiOutput::Native {
            progress, reader, ..
        } = self.output.as_ref()
        else {
            return Ok(());
        };
        let description = reader.description();
        let mut stored = self
            .native_progress
            .lock()
            .map_err(|_| anyhow::anyhow!("native query metrics poisoned"))?;
        let mut next = stored
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("native query progress was not initialized"))?;
        let previous = next.clone();
        if let Some(observation) = observation {
            observation.validate(&self.config.id, &description)?;
            anyhow::ensure!(
                observation.generation >= previous.generation,
                "native query observation regressed its output generation"
            );
            if observation.generation == previous.generation {
                previous.cursor.ensure_same_epoch(&observation.cursor)?;
                anyhow::ensure!(
                    observation.cursor.sequence() >= previous.cursor.sequence(),
                    "native query observation regressed its committed output"
                );
            }
            next.cursor = observation.cursor.clone();
            next.generation = observation.generation;
        }
        let mut emitted = false;
        for output in outputs
            .iter()
            .filter(|output| output.envelope.system().stream() == &description.output_stream)
        {
            emitted = true;
            let identity = super::QueryRecoveryIdentity::from_query_output(&output.envelope)?;
            anyhow::ensure!(
                identity.query_id() == &self.config.id
                    && identity.graph_id() == description.graph_id
                    && identity.configuration_hash() == description.configuration_hash,
                "query progress output belongs to a different configuration"
            );
            let producer = super::GraphProducerProgress::from_query_envelope(&output.envelope)?
                .ok_or_else(|| anyhow::anyhow!("query progress output has no producer identity"))?;
            let sequence = super::QueryChangeCodec::query_sequence(&output.envelope)?;
            let generation = super::QueryChangeCodec::query_generation(&output.envelope)?;
            anyhow::ensure!(
                sequence != 0 && producer.sequence() == sequence,
                "native query output has inconsistent logical progress"
            );
            if observation.is_none() && generation > next.generation {
                next.cursor = OutputCursor::try_new(producer.identity().clone(), sequence)?;
                next.generation = generation;
            } else {
                anyhow::ensure!(
                    generation == next.generation && producer.identity() == next.cursor.producer(),
                    "native query output belongs to another output generation"
                );
                if observation.is_some() {
                    anyhow::ensure!(
                        sequence <= next.cursor.sequence(),
                        "native query observation precedes its emitted output"
                    );
                } else if sequence > next.cursor.sequence() {
                    next.cursor = next.cursor.at_sequence(sequence);
                }
            }
        }
        anyhow::ensure!(
            observation.is_some() == (emitted && reader.supports_observations()),
            "native query output observation does not match its negotiated emission"
        );
        if let Some(observation) = observation {
            self.record_native_observation(observation)?;
        }
        let advances = if next.generation == previous.generation {
            next.cursor.sequence() - previous.cursor.sequence()
        } else {
            next.cursor.sequence()
        };
        self.metrics.record_seq_advances(advances);
        next.input = progress.as_ref().map(|progress| progress.snapshot());
        *stored = Some(next);
        Ok(())
    }

    pub async fn output_frontier(&self) -> anyhow::Result<(u64, u64, bool)> {
        let (sequence, generation, incarnation) = self.output.head().await?;
        Ok((sequence, generation, incarnation.is_none()))
    }

    pub(crate) fn reads_require_activation(&self) -> bool {
        matches!(self.output.as_ref(), QueryApiOutput::Native { .. })
    }

    /// Attach read-only query APIs to an actual component-provided reader.
    /// No evaluator, storage owner or duplicate result projection is created.
    pub fn from_native_reader(
        id: ComponentId,
        reader: Arc<dyn QueryViewProvider>,
        progress: Option<Arc<super::QuerySourceProgress>>,
    ) -> anyhow::Result<Arc<Self>> {
        let description = reader.description();
        drasi_computation_contracts::validate_identifier("query graph", &description.graph_id)?;
        description.execution.validate(None)?;
        anyhow::ensure!(
            progress.as_ref().is_none_or(|progress| {
                progress.graph_id() == description.graph_id && progress.component_id() == &id
            }),
            "query read progress belongs to another graph or component"
        );
        let definition = super::ContinuousQueryDefinition {
            graph_id: description.graph_id.clone(),
            id,
            query: description.query.clone(),
            language: description.language,
            output_stream: description.output_stream.clone(),
            outbox_capacity: description.outbox_capacity,
        };
        anyhow::ensure!(
            definition.configuration_hash(&description.execution)?
                == description.configuration_hash,
            "native query description has an inconsistent configuration identity"
        );
        let config = super::query_configuration::query_configuration(
            &description.execution,
            &definition,
            description.bootstrap,
            if description.auto_reset {
                super::QueryRecoveryPolicy::AutoReset
            } else {
                super::QueryRecoveryPolicy::Strict
            },
        )?;
        Ok(Arc::new(Self {
            config,
            catalog: None,
            metrics: Arc::new(QueryOutputMetrics::new()),
            native_progress: Default::default(),
            output: Arc::new(QueryApiOutput::Native { reader, progress }),
            _registration: None,
            links: std::sync::Mutex::new(Vec::new()),
        }))
    }

    /// Bind the native read capability to an existing graph-owned result outlet.
    /// Channel/broadcast delivery stays in that outlet, not in the native read ABI.
    pub fn with_native_result_catalog(
        &self,
        catalog: QueryResultsCatalog,
        mode: crate::DispatchMode,
        capacity: usize,
    ) -> anyhow::Result<Arc<Self>> {
        let mut config = self.config.clone();
        config.dispatch_mode = Some(mode);
        config.dispatch_buffer_capacity = Some(capacity);
        self.with_resolved_configuration(config, Some(catalog), capacity)
    }

    /// Keep the ordinary declaration intact while binding the plugin's actual
    /// read capability to the existing result outlet.
    pub fn with_native_configuration(
        &self,
        config: AssignedQueryConfig,
        catalog: Option<QueryResultsCatalog>,
        default_dispatch_capacity: usize,
    ) -> anyhow::Result<Arc<Self>> {
        self.with_resolved_configuration(
            config.into_resolved()?,
            catalog,
            default_dispatch_capacity,
        )
    }

    fn with_resolved_configuration(
        &self,
        mut config: QueryConfig,
        catalog: Option<QueryResultsCatalog>,
        default_dispatch_capacity: usize,
    ) -> anyhow::Result<Arc<Self>> {
        let QueryApiOutput::Native { reader, .. } = self.output.as_ref() else {
            anyhow::bail!("linked queries bind their catalogue on the executing transformer");
        };
        let description = reader.description();
        anyhow::ensure!(
            self.catalog.is_none(),
            "query result catalogue is already bound"
        );
        anyhow::ensure!(
            config.id == self.config.id,
            "query declaration belongs to another component"
        );
        config.source_transactions = description.execution.source_transactions;
        let execution = config.execution_settings();
        let definition = super::ContinuousQueryDefinition {
            graph_id: description.graph_id.clone(),
            id: config.id.clone(),
            query: config.query.clone(),
            language: match config.query_language {
                crate::config::QueryLanguage::Cypher => super::ComputationQueryLanguage::Cypher,
                crate::config::QueryLanguage::GQL => super::ComputationQueryLanguage::Gql,
            },
            output_stream: description.output_stream.clone(),
            outbox_capacity: description.outbox_capacity,
        };
        anyhow::ensure!(
            definition.configuration_hash(&execution)? == description.configuration_hash,
            "ordinary query configuration differs from the native query"
        );
        let registration = catalog
            .as_ref()
            .map(|catalog| {
                anyhow::ensure!(
                    catalog.graph_id() == description.graph_id,
                    "query result catalogue belongs to another graph"
                );
                catalog.configure_delivery(
                    &config,
                    self.metrics.clone(),
                    default_dispatch_capacity,
                )?;
                catalog.register_output(config.id.clone(), self.output.clone())
            })
            .transpose()?;
        Ok(Arc::new(Self {
            config,
            catalog,
            metrics: self.metrics.clone(),
            native_progress: self.native_progress.clone(),
            output: self.output.clone(),
            _registration: registration,
            links: std::sync::Mutex::new(Vec::new()),
        }))
    }

    pub(crate) fn progress_metrics(
        &self,
        component: &super::ObservedComponent,
    ) -> anyhow::Result<Option<crate::metrics::QueryProgressMetricsSnapshot>> {
        if let QueryApiOutput::Native {
            reader, progress, ..
        } = self.output.as_ref()
        {
            let description = reader.description();
            let Some(progress) = progress else {
                return Ok(None);
            };
            let current = progress.snapshot();
            if !current.ready || !current.bootstrap_complete || current.failure.is_some() {
                return Ok(None);
            }
            let cached = self
                .native_progress
                .lock()
                .map_err(|_| anyhow::anyhow!("native query metrics poisoned"))?
                .clone();
            let Some(cached) = cached else {
                return Ok(None);
            };
            let Some(input) = &cached.input else {
                return Ok(None);
            };
            if input.reset_generation != current.reset_generation
                || !input.ready
                || input.failure.is_some()
            {
                return Ok(None);
            }
            return Ok(Some(crate::metrics::QueryProgressMetricsSnapshot {
                query_id: self.config.id.clone(),
                component_generation: component.generation.0,
                component_operation: component.operation.0,
                output_generation: cached.generation,
                config_hash: description.configuration_hash,
                persistent: cached.cursor.producer().persistent() && input.persistent,
                input_sequences: input
                    .checkpoints
                    .iter()
                    .map(|(key, checkpoint)| (key.clone(), checkpoint.sequence))
                    .collect(),
                output_sequence: cached.cursor.sequence(),
            }));
        }
        let QueryApiOutput::Local(output) = self.output.as_ref() else {
            unreachable!()
        };
        let Some(progress) = &output.progress else {
            return Ok(None);
        };
        let progress = progress.snapshot();
        if !progress.ready || !progress.bootstrap_complete || progress.failure.is_some() {
            return Ok(None);
        }
        let snapshot = output.results.snapshot()?;
        if progress.reset_generation != snapshot.generation {
            return Ok(None);
        }
        Ok(Some(crate::metrics::QueryProgressMetricsSnapshot {
            query_id: self.config.id.clone(),
            component_generation: component.generation.0,
            component_operation: component.operation.0,
            output_generation: snapshot.generation,
            config_hash: output.config_hash,
            persistent: progress.persistent && self.is_persistent()?,
            input_sequences: progress
                .checkpoints
                .iter()
                .map(|(key, checkpoint)| (key.clone(), checkpoint.sequence))
                .collect(),
            output_sequence: snapshot.as_of_sequence,
        }))
    }

    pub(crate) fn link_catalog(&self, target: &QueryResultsCatalog) -> anyhow::Result<()> {
        let source = self.catalog.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Query '{}' does not expose result subscriptions",
                self.config.id
            )
        })?;
        let mut links = self
            .links
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue links poisoned"))?;
        if !links
            .iter()
            .any(|(catalog, _)| catalog.same_registry(target))
        {
            links.push((target.clone(), target.link_query(&self.config.id, source)?));
        }
        Ok(())
    }

    pub(crate) async fn snapshot(&self, timeout: Duration) -> anyhow::Result<SnapshotResponse> {
        let snapshot = tokio::time::timeout(timeout, self.output.snapshot()).await??;
        self.metrics.record_snapshot_fetch();
        super::plugin_reaction::snapshot_response_parts(
            &self.config.id,
            &snapshot,
            self.output.configuration_hash(),
        )
    }

    pub(crate) async fn outbox(
        &self,
        after: u64,
        timeout: Duration,
    ) -> Result<OutboxResponse, FetchError> {
        let output = match self.output.as_ref() {
            QueryApiOutput::Local(output) => output,
            QueryApiOutput::Native { reader, .. } => {
                let description = reader.description();
                let read = async {
                    let (head, generation) = reader.query_head().await?;
                    if after == u64::MAX {
                        return Ok((head.sequence(), generation, Vec::new()));
                    }
                    let (replay, generation) =
                        reader.query_replay(&head.at_sequence(after)).await?;
                    Ok::<_, OutputReadError>((
                        replay.head().sequence(),
                        generation,
                        replay.into_envelopes(),
                    ))
                };
                let (head, generation, retained) = tokio::time::timeout(timeout, read)
                    .await
                    .map_err(|_| FetchError::TimedOut)?
                    .map_err(|error| match error {
                        OutputReadError::Gap {
                            requested,
                            oldest,
                            latest,
                        } => FetchError::OutboxGap(crate::queries::output_state::OutboxGap {
                            requested,
                            earliest_available: oldest.unwrap_or(latest),
                            latest_sequence: latest,
                            config_hash: description.configuration_hash,
                        }),
                        error => {
                            log::error!("Cannot read native query history: {error:#}");
                            FetchError::NotRunning {
                                status: crate::ComponentStatus::Error,
                            }
                        }
                    })?;
                return super::plugin_reaction::outbox_response_parts(
                    &retained,
                    head,
                    generation,
                    description.configuration_hash,
                    after,
                );
            }
        };
        tokio::time::timeout(timeout, output.results.wait_ready())
            .await
            .map_err(|_| FetchError::TimedOut)?
            .map_err(|error| {
                log::error!("Query output is unavailable: {error:#}");
                FetchError::NotRunning {
                    status: crate::ComponentStatus::Error,
                }
            })?;
        let view = output.reader.query_view().map_err(|error| {
            log::error!("Cannot read query output: {error:#}");
            FetchError::NotRunning {
                status: crate::ComponentStatus::Error,
            }
        })?;
        super::plugin_reaction::outbox_response(&view, after)
    }

    pub(crate) fn is_persistent(&self) -> anyhow::Result<bool> {
        self.output.is_persistent()
    }

    pub(crate) async fn generation(&self) -> anyhow::Result<u64> {
        match self.output.as_ref() {
            QueryApiOutput::Local(output) => Ok(output.results.snapshot()?.generation),
            QueryApiOutput::Native { reader, .. } => {
                let observed = self
                    .native_progress
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native query metrics poisoned"))?
                    .as_ref()
                    .map(|progress| progress.generation);
                match observed {
                    Some(generation) => Ok(generation),
                    None => Ok(reader.query_head().await?.1),
                }
            }
        }
    }

    pub(crate) async fn validate_subscription(
        &self,
        head: &super::query_catalog::QuerySubscriptionHead,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.output.same_owner(&head.output),
            "query output owner changed; recreate the subscription"
        );
        let (_, generation, incarnation) = self.output.head().await?;
        anyhow::ensure!(
            generation == head.generation
                && incarnation == head.incarnation
                && self.output.configuration_hash() == head.config_hash,
            "query output generation changed; recreate the subscription"
        );
        Ok(())
    }
}

impl QueryApiOutput {
    pub(super) fn same_owner(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(left), Self::Local(right)) => left.results.same_owner(&right.results),
            (Self::Native { reader: left, .. }, Self::Native { reader: right, .. }) => {
                Arc::ptr_eq(left, right)
            }
            _ => false,
        }
    }

    pub(super) fn configuration_hash(&self) -> u64 {
        match self {
            Self::Local(output) => output.config_hash,
            Self::Native { reader, .. } => reader.description().configuration_hash,
        }
    }

    pub(super) async fn wait_ready(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(output) => {
                if let Some(progress) = &output.progress {
                    progress.wait_ready().await?;
                }
                output.results.wait_ready().await?;
            }
            Self::Native {
                reader, progress, ..
            } => {
                if let Some(progress) = progress {
                    progress.wait_ready().await?;
                }
                reader.query_head().await?;
            }
        }
        Ok(())
    }

    pub(super) async fn head(&self) -> anyhow::Result<(u64, u64, Option<uuid::Uuid>)> {
        match self {
            Self::Local(output) => {
                let head = output.results.head()?;
                anyhow::ensure!(head.ready, "query output is not ready for subscriptions");
                Ok((
                    head.sequence,
                    head.generation,
                    *output
                        .incarnation
                        .read()
                        .map_err(|_| anyhow::anyhow!("query output identity poisoned"))?,
                ))
            }
            Self::Native { reader, .. } => {
                let (cursor, generation) = reader.query_head().await?;
                Ok((
                    cursor.sequence(),
                    generation,
                    (!cursor.producer().persistent()).then_some(cursor.producer().incarnation()),
                ))
            }
        }
    }

    pub(super) async fn view(&self) -> anyhow::Result<QueryOutputView> {
        match self {
            Self::Local(output) => Ok(output.reader.query_view()?),
            Self::Native { reader, .. } => Ok(reader.query_view().await?.1),
        }
    }

    pub(super) async fn snapshot(&self) -> anyhow::Result<QueryResultSnapshot> {
        match self {
            Self::Local(output) => {
                output.results.wait_ready().await?;
                Ok(output.reader.query_view()?.snapshot)
            }
            Self::Native { reader, .. } => {
                let (snapshot, generation) = reader.query_snapshot().await?;
                let description = reader.description();
                Ok(QueryResultSnapshot {
                    projector: ResultNodeProjector::try_new(
                        snapshot.dataset(),
                        description.execution.result.label.as_deref(),
                    )?,
                    as_of_sequence: snapshot.cursor().sequence(),
                    generation,
                    rows: snapshot.records().clone(),
                })
            }
        }
    }

    pub(super) async fn replay(&self, after: u64) -> anyhow::Result<Vec<ChangeEnvelope>> {
        let replay = match self {
            Self::Local(output) => {
                let (head, _) = output.reader.query_head()?;
                output.reader.query_replay(&head.at_sequence(after))?.0
            }
            Self::Native { reader, .. } => {
                let (head, _) = reader.query_head().await?;
                reader.query_replay(&head.at_sequence(after)).await?.0
            }
        };
        Ok(replay.into_envelopes())
    }

    pub(super) fn is_persistent(&self) -> anyhow::Result<bool> {
        let output = match self {
            QueryApiOutput::Local(output) => output,
            QueryApiOutput::Native { reader, .. } => return Ok(!reader.description().volatile),
        };
        output
            .output_persistence
            .read()
            .map_err(|_| anyhow::anyhow!("query persistence observation poisoned"))?
            .ok_or_else(|| anyhow::anyhow!("query output persistence is not yet known"))
    }
}
