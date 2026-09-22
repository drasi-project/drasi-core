// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroUsize,
    sync::Arc,
};

use bytes::Bytes;
use drasi_core::{
    computation::{ComputationIndexes, ComputationTransaction},
    interface::{ElementIndex, IndexError, SourceCheckpoint},
    middleware::SourceMiddlewarePipeline,
    models::SourceChange,
};
use serde::{Deserialize, Serialize};

use super::{
    producer_progress::{GraphInputProgress, INPUT_OWNER_PREFIX},
    ChangeEnvelope, EnvelopeCodec, GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress,
    MiddlewareTransformerDefinition, OutputEnvelope, SourceProgressKey, SourceProgressSnapshot,
};

const CONFIGURATION: &str = "\0computation:middleware-configuration:v1";
const HEAD: &str = "\0computation:middleware-head:v1";
const EMISSION: &str = "\0computation:middleware-emission:v1";
const CONFIRMED: &str = "\0computation:middleware-confirmed:v1";
const JOURNAL: &str = "computation:middleware-output:v1";
const INPUT_OUTPUT_PREFIX: &str = "\0computation:middleware-input-output:v1:";
const MAX_ENVELOPE_BYTES: usize = 64 * 1024 * 1024;

/// Durable middleware owns an isolated atomic bundle within this graph scope.
/// Capacity counts transformed input batches, including empty progress batches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableMiddlewareOptions {
    pub graph_id: String,
    pub outbox_capacity: NonZeroUsize,
}

#[derive(Debug, thiserror::Error)]
pub enum MiddlewareRecoveryError {
    #[error("middleware retention is full; deliver and confirm pending output before retrying")]
    RetentionExhausted,
    #[error("durable middleware configuration or ownership changed; use a separate empty scope")]
    ConfigurationChanged,
    #[error("inconsistent durable middleware state: {0}")]
    Inconsistent(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    version: u32,
    definition: serde_json::Value,
    identity: GraphProducerIdentity,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputReceipt {
    key: String,
    sequence: u64,
    position: Option<Vec<u8>>,
    changes: Vec<u8>,
}

impl InputReceipt {
    fn new(progress: &GraphInputProgress, changes: &[SourceChange]) -> anyhow::Result<Self> {
        let changes = rmp_serde::to_vec_named(changes)?;
        anyhow::ensure!(
            changes.len() <= MAX_ENVELOPE_BYTES,
            "middleware input exceeds the retained envelope size limit"
        );
        Ok(Self {
            key: progress.key.clone(),
            sequence: progress.sequence,
            position: progress.position.as_ref().map(|position| position.to_vec()),
            changes,
        })
    }

    fn matches(
        &self,
        progress: &GraphInputProgress,
        changes: &[SourceChange],
    ) -> anyhow::Result<bool> {
        Ok(self.key == progress.key
            && self.sequence == progress.sequence
            && self.position.as_deref() == progress.position.as_deref()
            && rmp_serde::from_slice::<Vec<SourceChange>>(&self.changes)? == changes)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalRecord {
    version: u32,
    input: InputReceipt,
    envelope: Vec<u8>,
}

struct RetainedOutput {
    input: InputReceipt,
    logical_sequence: u64,
    envelope: ChangeEnvelope,
}

struct Recovered {
    identity: GraphProducerIdentity,
    head: u64,
    emission: u64,
    confirmed: u64,
    retained: VecDeque<RetainedOutput>,
    checkpoints: BTreeMap<SourceProgressKey, SourceCheckpoint>,
}

pub(super) struct MiddlewareStore {
    pub transaction: ComputationTransaction,
    options: DurableMiddlewareOptions,
    scope: String,
    definition: MiddlewareTransformerDefinition,
    configuration: serde_json::Value,
    codec: EnvelopeCodec,
    state: Option<Recovered>,
}

fn index_error(error: anyhow::Error) -> IndexError {
    IndexError::Other(error.into_boxed_dyn_error())
}

impl MiddlewareStore {
    pub fn new(
        indexes: ComputationIndexes,
        definition: &MiddlewareTransformerDefinition,
        options: DurableMiddlewareOptions,
        scope: String,
    ) -> anyhow::Result<Self> {
        super::data::validate_identifier("middleware graph", &options.graph_id)?;
        super::data::validate_identifier("middleware construction scope", &scope)?;
        indexes.atomic_result_transaction()?;
        anyhow::ensure!(
            indexes.cleanup().is_some(),
            "durable middleware requires an explicit asynchronous cleanup owner"
        );
        anyhow::ensure!(
            indexes
                .checkpoint_store()
                .is_some_and(|store| store.is_persistent()),
            "durable middleware requires persistent checkpoints and indexes"
        );
        let configuration = serde_json::to_value((
            1u32,
            "drasi/middleware-transformer",
            "1",
            &scope,
            &options,
            definition.id.as_str(),
            definition.output_stream.as_str(),
            &definition.middleware,
            &definition.pipeline,
        ))?;
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(MAX_ENVELOPE_BYTES).expect("limit"));
        codec.register_schema(GraphChangeCodec::schema())?;
        Ok(Self {
            transaction: ComputationTransaction::try_new(indexes)?,
            options,
            scope,
            definition: definition.clone(),
            configuration,
            codec,
            state: None,
        })
    }

    pub fn graph_id(&self) -> &str {
        &self.options.graph_id
    }

    pub fn elements(&self) -> Arc<dyn ElementIndex> {
        self.transaction.resources().indexes().element_index.clone()
    }

    fn state(&self) -> anyhow::Result<&Recovered> {
        self.state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("middleware recovery has not completed"))
    }

    pub fn pending(&self) -> anyhow::Result<VecDeque<u64>> {
        let state = self.state()?;
        Ok(state
            .retained
            .iter()
            .filter(|entry| entry.logical_sequence > state.confirmed)
            .map(|entry| entry.logical_sequence)
            .collect())
    }

    pub fn progress(&self) -> anyhow::Result<SourceProgressSnapshot> {
        Ok(SourceProgressSnapshot {
            ready: true,
            admitting: true,
            recovered: true,
            bootstrap_complete: true,
            persistent: true,
            checkpoints: self.state()?.checkpoints.clone(),
            ..Default::default()
        })
    }

    pub async fn recover(&mut self) -> anyhow::Result<()> {
        self.transaction.quiesce().await?;
        let recovered = self
            .transaction
            .run(async { self.load().await.map_err(index_error).map_err(Into::into) })
            .await?;
        self.state = Some(recovered);
        Ok(())
    }

    async fn load(&self) -> anyhow::Result<Recovered> {
        let resources = self.transaction.resources();
        let checkpoint = resources.checkpoint_store().expect("validated checkpoint");
        let outbox = resources.outbox_writer().expect("validated outbox");
        let saved = checkpoint.read_all_checkpoints().await?;
        let entries = outbox.read_from(JOURNAL, 0).await?;
        let (identity, head, emission, confirmed) = if let Some(config) = saved.get(CONFIGURATION) {
            let config: Configuration = serde_json::from_slice(
                config
                    .source_position
                    .as_deref()
                    .filter(|_| config.sequence == 1)
                    .ok_or_else(|| anyhow::anyhow!("invalid middleware configuration marker"))?,
            )?;
            config.identity.validate()?;
            if config.version != 1
                || config.definition != self.configuration
                || config.identity.construction_scope() != self.scope
                || config.identity.graph_id() != self.options.graph_id
                || config.identity.component_id() != &self.definition.id
                || config.identity.stream() != &self.definition.output_stream
                || !config.identity.persistent()
            {
                return Err(MiddlewareRecoveryError::ConfigurationChanged.into());
            }
            let number = |key| -> anyhow::Result<u64> {
                Ok(saved
                    .get(key)
                    .filter(|checkpoint| checkpoint.source_position.is_none())
                    .ok_or_else(|| anyhow::anyhow!("missing middleware sequence marker"))?
                    .sequence)
            };
            (
                config.identity,
                number(HEAD)?,
                number(EMISSION)?,
                number(CONFIRMED)?,
            )
        } else {
            anyhow::ensure!(
                saved.is_empty()
                    && entries.is_empty()
                    && checkpoint.read_config_hash().await?.is_none()
                    && checkpoint
                        .read_result_sequence(self.definition.id.as_str())
                        .await?
                        .is_none()
                    && outbox
                        .read_from(self.definition.id.as_str(), 0)
                        .await?
                        .is_empty()
                    && resources
                        .live_results_writer()
                        .expect("validated projection")
                        .read_snapshot(self.definition.id.as_str())
                        .await?
                        .is_empty(),
                "refusing to adopt unowned or another component's index storage"
            );
            let identity = GraphProducerIdentity::new(
                self.scope.clone(),
                self.options.graph_id.clone(),
                self.definition.id.clone(),
                self.definition.output_stream.clone(),
                true,
            )?;
            let bytes = Bytes::from(serde_json::to_vec(&Configuration {
                version: 1,
                definition: self.configuration.clone(),
                identity: identity.clone(),
            })?);
            checkpoint
                .stage_checkpoint(CONFIGURATION, 1, Some(&bytes))
                .await?;
            for key in [HEAD, EMISSION, CONFIRMED] {
                checkpoint.stage_checkpoint(key, 0, None).await?;
            }
            (identity, 0, 0, 0)
        };
        anyhow::ensure!(
            emission >= head && confirmed <= head,
            "invalid middleware high-watermarks"
        );
        anyhow::ensure!(
            entries.len() <= self.options.outbox_capacity.get(),
            "middleware journal exceeds configured retention"
        );
        let mut retained = VecDeque::new();
        let mut previous = None;
        let mut previous_emission = None;
        for (logical_sequence, bytes) in entries {
            let record: JournalRecord = serde_json::from_slice(&bytes)?;
            let envelope = self.codec.decode(&record.envelope)?;
            let progress = GraphProducerProgress::from_envelope(&envelope)?
                .ok_or_else(|| anyhow::anyhow!("retained middleware output has no identity"))?;
            anyhow::ensure!(
                record.version == 1
                    && progress.identity() == &identity
                    && progress.sequence() == logical_sequence
                    && envelope.system().sequence() <= emission
                    && envelope.system().sequence() >= logical_sequence
                    && previous_emission
                        .map_or(true, |previous| envelope.system().sequence() > previous)
                    && envelope.id()
                        == &super::emission_id(
                            &self.definition.output_stream,
                            envelope.system().sequence()
                        )?
                    && envelope.lineage().is_some()
                    && logical_sequence <= head
                    && previous.map_or(
                        logical_sequence.saturating_sub(1) <= confirmed,
                        |previous: u64| previous.checked_add(1) == Some(logical_sequence)
                    ),
                "inconsistent middleware journal identity or sequence"
            );
            super::query::progress_identity(&record.input.key)?;
            let input = saved
                .get(&record.input.key)
                .ok_or_else(|| anyhow::anyhow!("middleware output has no committed input"))?;
            anyhow::ensure!(
                input.sequence >= record.input.sequence,
                "middleware output is ahead of committed input"
            );
            let _: Vec<SourceChange> = rmp_serde::from_slice(&record.input.changes)?;
            retained.push_back(RetainedOutput {
                input: record.input,
                logical_sequence,
                envelope,
            });
            previous = Some(logical_sequence);
            previous_emission = retained
                .back()
                .map(|entry| entry.envelope.system().sequence());
        }
        anyhow::ensure!(
            previous == (head != 0).then_some(head),
            "middleware journal does not reach its committed head"
        );
        let mut checkpoints = BTreeMap::new();
        for (key, checkpoint) in &saved {
            if key.starts_with("computation:input:") {
                let mapping = saved
                    .get(&format!("{INPUT_OUTPUT_PREFIX}{key}"))
                    .ok_or_else(|| {
                        anyhow::anyhow!("middleware input has no committed output identity")
                    })?;
                anyhow::ensure!(
                    mapping.sequence > 0
                        && mapping.sequence <= head
                        && mapping.source_position.is_none(),
                    "invalid middleware committed input/output identity"
                );
                if let Some(entry) = retained
                    .iter()
                    .find(|entry| entry.logical_sequence == mapping.sequence)
                {
                    anyhow::ensure!(
                        entry.input.key == *key
                            && entry.input.sequence == checkpoint.sequence
                            && entry.input.position.as_deref()
                                == checkpoint.source_position.as_deref(),
                        "middleware input checkpoint contradicts retained output"
                    );
                } else {
                    anyhow::ensure!(
                        mapping.sequence <= confirmed
                            && retained
                                .front()
                                .is_some_and(|entry| mapping.sequence < entry.logical_sequence),
                        "middleware input's unconfirmed output is missing"
                    );
                }
                checkpoints.insert(super::query::progress_identity(key)?, checkpoint.clone());
            } else if let Some(input) = key.strip_prefix(INPUT_OWNER_PREFIX) {
                let owner: GraphProducerIdentity = serde_json::from_slice(
                    checkpoint
                        .source_position
                        .as_deref()
                        .filter(|_| checkpoint.sequence == 1)
                        .ok_or_else(|| anyhow::anyhow!("invalid middleware input owner marker"))?,
                )?;
                owner.validate()?;
                anyhow::ensure!(
                    owner.persistent()
                        && input == super::query::progress_key(owner.stream().as_str(), None)
                        && saved.contains_key(input),
                    "middleware input owner does not match its committed stream"
                );
            } else {
                anyhow::ensure!(
                    [CONFIGURATION, HEAD, EMISSION, CONFIRMED].contains(&key.as_str())
                        || key
                            .strip_prefix(INPUT_OUTPUT_PREFIX)
                            .is_some_and(|input| saved.contains_key(input)),
                    "middleware storage contains another component's checkpoint"
                );
            }
        }
        Ok(Recovered {
            identity,
            head,
            emission,
            confirmed,
            retained,
            checkpoints,
        })
    }

    pub async fn replay(&mut self, logical_sequence: u64) -> anyhow::Result<ChangeEnvelope> {
        let state = self.state()?;
        let entry = state
            .retained
            .iter()
            .find(|entry| entry.logical_sequence == logical_sequence)
            .ok_or_else(|| anyhow::anyhow!("pending middleware output is unavailable"))?;
        let emission = state
            .emission
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("middleware emission sequence exhausted"))?;
        let envelope = entry.envelope.reemit(emission)?;
        let checkpoint = self
            .transaction
            .resources()
            .checkpoint_store()
            .expect("checkpoint");
        self.transaction
            .run(async {
                checkpoint
                    .stage_checkpoint(EMISSION, emission, None)
                    .await?;
                Ok(())
            })
            .await?;
        self.state.as_mut().expect("recovered").emission = emission;
        Ok(envelope)
    }

    pub async fn process(
        &mut self,
        input: &ChangeEnvelope,
        changes: Vec<SourceChange>,
        pipeline: &SourceMiddlewarePipeline,
    ) -> anyhow::Result<Option<ChangeEnvelope>> {
        let progress = GraphInputProgress::from_envelope(input)?;
        anyhow::ensure!(
            progress.position.as_ref().map_or(true, |position| {
                position.len() <= crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
            }),
            "middleware source position exceeds the supported checkpoint limit"
        );
        let checkpoint = self
            .transaction
            .resources()
            .checkpoint_store()
            .expect("checkpoint");
        let saved = progress.validate(checkpoint.as_ref(), true).await?;
        if saved.is_some_and(|saved| saved.sequence >= progress.sequence) {
            if let Some(entry) = self.state()?.retained.iter().find(|entry| {
                entry.input.key == progress.key && entry.input.sequence == progress.sequence
            }) {
                anyhow::ensure!(
                    entry.input.matches(&progress, &changes)?,
                    "replayed middleware input changed its committed contents or position"
                );
                return self.replay(entry.logical_sequence).await.map(Some);
            }
            // Only confirmed entries may be evicted. Their durable outgoing
            // pipes, rather than a second middleware evaluation, own redelivery.
            return Ok(None);
        }
        let state = self.state()?;
        let logical_sequence = state
            .head
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("middleware logical output sequence exhausted"))?;
        let emission = state
            .emission
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("middleware emission sequence exhausted"))?;
        let retain_from = logical_sequence
            .saturating_sub(self.options.outbox_capacity.get() as u64)
            .saturating_add(1);
        if retain_from.saturating_sub(1) > state.confirmed {
            return Err(MiddlewareRecoveryError::RetentionExhausted.into());
        }
        let receipt = InputReceipt::new(&progress, &changes)?;
        let elements = self.elements();
        let envelope = self
            .transaction
            .run(async {
                let prepared: anyhow::Result<ChangeEnvelope> = async {
                    let mut output = Vec::new();
                    for change in changes {
                        let transformed = pipeline.process(change, elements.clone()).await?;
                        super::middleware::remember(elements.as_ref(), &transformed).await?;
                        output.extend(transformed);
                    }
                    let mut envelope = GraphChangeCodec::derive_changes(
                        input,
                        &output,
                        self.definition.output_stream.clone(),
                        emission,
                    )?;
                    GraphProducerProgress::annotate(
                        &mut envelope,
                        &state.identity,
                        logical_sequence,
                    )?;
                    let bytes = serde_json::to_vec(&JournalRecord {
                        version: 1,
                        input: receipt.clone(),
                        envelope: self.codec.encode(&envelope)?.to_vec(),
                    })?;
                    self.transaction
                        .resources()
                        .outbox_writer()
                        .expect("outbox")
                        .append_and_trim(JOURNAL, logical_sequence, &bytes, retain_from)
                        .await?;
                    progress.stage(checkpoint.as_ref()).await?;
                    checkpoint
                        .stage_checkpoint(
                            &format!("{INPUT_OUTPUT_PREFIX}{}", progress.key),
                            logical_sequence,
                            None,
                        )
                        .await?;
                    checkpoint
                        .stage_checkpoint(HEAD, logical_sequence, None)
                        .await?;
                    checkpoint
                        .stage_checkpoint(EMISSION, emission, None)
                        .await?;
                    Ok(envelope)
                }
                .await;
                prepared.map_err(index_error).map_err(Into::into)
            })
            .await?;
        let state = self.state.as_mut().expect("recovered");
        state.head = logical_sequence;
        state.emission = emission;
        while state
            .retained
            .front()
            .is_some_and(|entry| entry.logical_sequence < retain_from)
        {
            state.retained.pop_front();
        }
        state.retained.push_back(RetainedOutput {
            input: receipt,
            logical_sequence,
            envelope: envelope.clone(),
        });
        state.checkpoints.insert(
            progress.identity,
            SourceCheckpoint::new(progress.sequence, progress.position),
        );
        Ok(Some(envelope))
    }

    pub async fn confirm(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        let state = self.state()?;
        let mut confirmed = state.confirmed;
        for output in outputs {
            let progress = GraphProducerProgress::from_envelope(&output.envelope)?
                .ok_or_else(|| anyhow::anyhow!("middleware delivery omitted producer progress"))?;
            anyhow::ensure!(
                output.port.as_str() == "out"
                    && progress.identity() == &state.identity
                    && output.envelope.system().sequence() <= state.emission
                    && progress.sequence() <= state.head,
                "middleware delivery confirmation belongs to another output"
            );
            if progress.sequence() > confirmed {
                anyhow::ensure!(
                    confirmed.checked_add(1) == Some(progress.sequence()),
                    "middleware delivery confirmation would skip pending output"
                );
                confirmed = progress.sequence();
            }
        }
        if confirmed != state.confirmed {
            let checkpoint = self
                .transaction
                .resources()
                .checkpoint_store()
                .expect("checkpoint");
            self.transaction
                .run(async {
                    checkpoint
                        .stage_checkpoint(CONFIRMED, confirmed, None)
                        .await?;
                    Ok(())
                })
                .await?;
            self.state.as_mut().expect("recovered").confirmed = confirmed;
        }
        Ok(())
    }
}
