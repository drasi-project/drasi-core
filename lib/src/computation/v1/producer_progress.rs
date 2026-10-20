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

use std::{collections::BTreeMap, sync::Arc};

use bytes::Bytes;
use drasi_core::interface::{CheckpointStore, IndexError, SourceCheckpoint};
use serde::{Deserialize, Serialize};

use super::{
    ChangeEnvelope, ComponentId, ContextEntry, ContextValue, GraphChangeCodec, SourceProgressKey,
    StreamId, SystemMetadata,
};

pub(super) const INPUT_OWNER_PREFIX: &str = "computation:input-owner:";

use drasi_computation_contracts::has_distinct_producer_lineage;
pub use drasi_computation_contracts::{GraphProducerIdentity, GraphProducerProgress};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueryInputOwner {
    version: u32,
    identity: super::QueryRecoveryIdentity,
    generation: u64,
}

impl QueryInputOwner {
    pub(super) fn validate(&self, persistent: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported query input identity version"
        );
        self.identity.validate()?;
        anyhow::ensure!(
            !persistent || self.identity.incarnation().is_none(),
            "transaction processing cannot recover through a volatile query"
        );
        Ok(())
    }
}

pub(super) struct GraphInputProgress {
    stream: StreamId,
    transport_sequence: Option<u64>,
    pub key: String,
    pub identity: SourceProgressKey,
    pub sequence: u64,
    pub position: Option<Bytes>,
    pub producer: Option<GraphProducerIdentity>,
    volatile_producer: bool,
    pub(super) positioned: Option<Box<PositionedInput>>,
}

pub(super) struct PositionedInput {
    pub policy: Option<Arc<super::InputPositionPolicy>>,
    producer: Option<GraphProducerIdentity>,
    origin_transport: Option<(StreamId, u64)>,
}

impl GraphInputProgress {
    pub(super) fn from_stream(input: &ChangeEnvelope, sequence: u64) -> Self {
        Self::from_system(input.system(), sequence)
    }

    fn from_system(system: &Arc<SystemMetadata>, sequence: u64) -> Self {
        let stream = system.stream().clone();
        Self {
            key: super::query::progress_key(stream.as_str(), None),
            identity: SourceProgressKey::Stream(stream.clone()),
            stream,
            transport_sequence: None,
            sequence,
            position: system.source_position().cloned(),
            producer: None,
            volatile_producer: false,
            positioned: None,
        }
    }

    pub fn from_envelope(input: &ChangeEnvelope) -> anyhow::Result<Self> {
        Self::from_boundary(input, input.system(), has_distinct_producer_lineage(input))
    }

    pub(super) fn from_processing_input(
        input: &ChangeEnvelope,
        progress_owner: Option<&super::QuerySourceProgress>,
        producer_role: Option<super::ComponentRole>,
    ) -> anyhow::Result<(Self, Option<QueryInputOwner>)> {
        let query_input =
            input.changes().schema() == super::QueryChangeCodec::schema().descriptor();
        let immediate = if query_input {
            GraphProducerProgress::from_query_envelope(input)?
        } else {
            GraphProducerProgress::from_envelope(input)?
        };
        if immediate.is_some() {
            return Ok((Self::from_envelope(input)?, None));
        }
        let fallback = SourceProgressKey::Stream(input.system().stream().clone());
        let bound_role = progress_owner
            .map(|owner| owner.input_binding(input, &fallback))
            .transpose()?
            .flatten()
            .map(|boundary| boundary.role);
        let query_producer =
            bound_role.or(producer_role) != Some(super::ComponentRole::Transformer);
        if query_input && query_producer {
            anyhow::ensure!(
                !super::QueryChangeCodec::is_snapshot(input)
                    && !super::QueryChangeCodec::is_progress_only(input),
                "tracked query inputs must be ordinary result events, not snapshot/skip decisions"
            );
            let owner = QueryInputOwner {
                version: 1,
                identity: super::QueryRecoveryIdentity::from_envelope(input)?,
                generation: super::QueryChangeCodec::query_generation(input)?,
            };
            owner.validate(false)?;
            anyhow::ensure!(
                super::QueryChangeCodec::metadata(input)?.query_id == owner.identity.query_id(),
                "query input metadata does not match its producer identity"
            );
            return Ok((
                Self::from_stream(input, super::QueryChangeCodec::query_sequence(input)?),
                Some(owner),
            ));
        }
        if input.changes().schema() == GraphChangeCodec::schema().descriptor()
            || input.changes().schema() == super::SourceTransactionCodec::schema().descriptor()
        {
            Ok((Self::from_envelope(input)?, None))
        } else {
            let mut progress = Self::from_stream(input, input.system().sequence());
            if query_input {
                progress.position = None;
            }
            Ok((progress, None))
        }
    }

    fn from_boundary(
        input: &ChangeEnvelope,
        system: &Arc<SystemMetadata>,
        distinct_lineage: bool,
    ) -> anyhow::Result<Self> {
        let (producer, inherited_volatile) = if Arc::ptr_eq(system, input.system()) {
            GraphProducerProgress::read_scoped(input, false)?
        } else {
            GraphProducerProgress::read_boundary(input, system)?
        };
        if let Some(progress) = producer.as_ref() {
            return Ok(Self {
                stream: system.stream().clone(),
                transport_sequence: None,
                key: super::query::progress_key(system.stream().as_str(), None),
                identity: SourceProgressKey::Stream(system.stream().clone()),
                sequence: progress.sequence(),
                position: None,
                producer: progress
                    .identity()
                    .persistent()
                    .then(|| progress.identity().clone()),
                volatile_producer: !progress.identity().persistent(),
                positioned: None,
            });
        }
        if distinct_lineage {
            let mut progress = Self::from_system(system, system.sequence());
            // Inherited source metadata is causal history, not this producer's cursor.
            progress.position = None;
            // Lineage alone does not declare durability; an explicit volatile
            // ancestor still cannot become a durable boundary by derivation.
            progress.volatile_producer = inherited_volatile;
            return Ok(progress);
        }
        if let Some((source, sequence)) = super::SourceTransactionCodec::replay_progress(input)? {
            return Ok(Self {
                stream: system.stream().clone(),
                transport_sequence: Some(system.sequence()),
                key: super::query::progress_key(system.stream().as_str(), Some(&source)),
                identity: SourceProgressKey::Source(source),
                sequence,
                position: system.source_position().cloned(),
                producer: None,
                volatile_producer: false,
                positioned: None,
            });
        }
        let raw = GraphChangeCodec::source_metadata(input)?;
        let stable = raw.as_ref().and_then(|metadata| {
            metadata
                .sequence
                .map(|sequence| (metadata.source_id.as_str(), sequence))
        });
        Ok(Self {
            stream: system.stream().clone(),
            transport_sequence: stable.map(|_| system.sequence()),
            key: super::query::progress_key(
                system.stream().as_str(),
                stable.map(|(source, _)| source),
            ),
            identity: match stable {
                Some((source, _)) => SourceProgressKey::Source(source.to_owned()),
                None => SourceProgressKey::Stream(system.stream().clone()),
            },
            sequence: stable.map_or(system.sequence(), |(_, sequence)| sequence),
            position: raw
                .as_ref()
                .and_then(|metadata| metadata.source_position.as_ref())
                .map(|position| Bytes::copy_from_slice(position))
                .or_else(|| system.source_position().cloned()),
            producer: None,
            volatile_producer: false,
            positioned: None,
        })
    }

    pub(super) fn with_consumer_boundary(
        mut self,
        input: &ChangeEnvelope,
        owner: Option<&super::QuerySourceProgress>,
    ) -> anyhow::Result<Self> {
        let Some(owner) = owner else {
            return Ok(self);
        };
        let Some(boundary) = owner.input_binding(input, &self.identity)? else {
            return Ok(self);
        };
        let origin = boundary.system;
        let producer = GraphProducerProgress::read_boundary(input, &origin)?.0;
        let query_progress = boundary.role != super::ComponentRole::Transformer
            && input.changes().schema() == super::QueryChangeCodec::schema().descriptor()
            && producer.is_none();
        if origin.stream() != input.system().stream() {
            self = if query_progress {
                let mut progress =
                    Self::from_system(&origin, super::QueryChangeCodec::query_sequence(input)?);
                progress.position = None;
                progress
            } else {
                Self::from_boundary(input, &origin, boundary.distinct_ancestor)?
            };
            self.stream = input.system().stream().clone();
        }
        if let Some(policy) = &boundary.policy {
            if &self.identity != policy.key() || self.position.is_none() {
                match policy.key() {
                    SourceProgressKey::Source(source) => {
                        if let Some((actual, sequence)) =
                            super::SourceTransactionCodec::replay_progress(input)?
                        {
                            anyhow::ensure!(
                                &actual == source,
                                "identity-preserving path changed its source identity"
                            );
                            self.sequence = sequence;
                            self.position = origin.source_position().cloned();
                        } else {
                            let metadata =
                                GraphChangeCodec::source_metadata(input)?.ok_or_else(|| {
                                    anyhow::anyhow!("input replay source metadata is missing")
                                })?;
                            anyhow::ensure!(
                                &metadata.source_id == source,
                                "identity-preserving path changed its source identity"
                            );
                            self.sequence = metadata.sequence.ok_or_else(|| {
                                anyhow::anyhow!("input replay source sequence is missing")
                            })?;
                            self.position = metadata
                                .source_position
                                .map(Bytes::from)
                                .or_else(|| origin.source_position().cloned());
                        }
                        self.key =
                            super::query::progress_key(origin.stream().as_str(), Some(source));
                        self.producer = None;
                    }
                    SourceProgressKey::Stream(stream) => {
                        anyhow::ensure!(stream == origin.stream(), "input replay stream changed");
                        self.sequence = if query_progress {
                            super::QueryChangeCodec::query_sequence(input)?
                        } else {
                            producer
                                .as_ref()
                                .map_or(origin.sequence(), GraphProducerProgress::sequence)
                        };
                        self.position = origin.source_position().cloned();
                        self.key = super::query::progress_key(stream.as_str(), None);
                    }
                }
                self.identity = policy.key().clone();
            }
        }
        self.transport_sequence = Some(input.system().sequence());
        self.positioned = Some(Box::new(PositionedInput {
            policy: boundary.policy,
            producer: producer
                .filter(|producer| {
                    self.identity == SourceProgressKey::Stream(producer.identity().stream().clone())
                })
                .map(|producer| producer.identity().clone()),
            origin_transport: (origin.stream() != &self.stream)
                .then(|| (origin.stream().clone(), origin.sequence())),
        }));
        Ok(self)
    }

    pub(super) fn with_transport(mut self, input: &ChangeEnvelope) -> Self {
        self.transport_sequence = Some(input.system().sequence());
        self
    }

    pub(super) fn input_producer(&self) -> Option<&GraphProducerIdentity> {
        self.producer.as_ref().or_else(|| {
            self.positioned
                .as_ref()
                .and_then(|positioned| positioned.producer.as_ref())
        })
    }

    fn owner_key(&self) -> String {
        let stream = self
            .positioned
            .as_ref()
            .and_then(|position| position.origin_transport.as_ref())
            .map_or(&self.stream, |(stream, _)| stream);
        format!(
            "{INPUT_OWNER_PREFIX}{}",
            super::query::progress_key(stream.as_str(), None)
        )
    }

    pub(super) fn validate_durability(&self, persistent: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !persistent || !self.volatile_producer,
            "persistent graph processing cannot recover through volatile middleware"
        );
        Ok(())
    }

    pub async fn validate(
        &self,
        store: &dyn CheckpointStore,
        persistent: bool,
    ) -> anyhow::Result<Option<SourceCheckpoint>> {
        let saved = store.read_checkpoint(&self.key).await?;
        let owner = store.read_checkpoint(&self.owner_key()).await?;
        anyhow::ensure!(
            self.producer.is_some() || owner.is_none(),
            "graph input omitted its committed producer identity"
        );
        self.validate_durability(persistent)?;
        match (&self.producer, owner) {
            (Some(identity), owner) => {
                anyhow::ensure!(
                    !persistent || identity.persistent(),
                    "persistent graph processing cannot recover through volatile middleware"
                );
                if let Some(owner) = owner {
                    anyhow::ensure!(
                        owner.sequence == 1,
                        "invalid input producer identity version"
                    );
                    let stored: GraphProducerIdentity = serde_json::from_slice(
                        owner
                            .source_position
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("missing input producer identity"))?,
                    )?;
                    anyhow::ensure!(&stored == identity, "graph input producer identity changed");
                } else {
                    anyhow::ensure!(saved.is_none(), "committed input has no producer identity");
                }
                if saved
                    .as_ref()
                    .map_or(true, |saved| self.sequence > saved.sequence)
                {
                    let expected = saved
                        .as_ref()
                        .map_or(Some(1), |saved| saved.sequence.checked_add(1));
                    anyhow::ensure!(
                        expected == Some(self.sequence),
                        "graph input logical progress has a gap"
                    );
                }
            }
            (None, Some(_)) => anyhow::bail!("graph input omitted its committed producer identity"),
            (None, None) => {}
        }
        Ok(saved)
    }

    pub async fn stage(&self, store: &dyn CheckpointStore) -> Result<(), IndexError> {
        if let Some(identity) = &self.producer {
            let bytes = Bytes::from(serde_json::to_vec(identity).map_err(IndexError::other)?);
            store
                .stage_checkpoint(&self.owner_key(), 1, Some(&bytes))
                .await?;
        }
        store
            .stage_checkpoint(&self.key, self.sequence, self.position.as_ref())
            .await?;
        self.stage_transport(store).await
    }

    pub async fn stage_transport(&self, store: &dyn CheckpointStore) -> Result<(), IndexError> {
        for (stream, sequence) in self.transport_positions() {
            let key = Self::transport_key(stream);
            let saved = store.read_checkpoint(&key).await?;
            if let Some(saved) = &saved {
                transport_identity(&key, saved)
                    .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
            }
            if saved
                .as_ref()
                .map_or(true, |saved| saved.sequence < sequence)
            {
                store.stage_checkpoint(&key, sequence, None).await?;
            }
        }
        Ok(())
    }

    fn transport_key(stream: &StreamId) -> String {
        format!(
            "{INPUT_TRANSPORT_PREFIX}{}",
            super::query::progress_key(stream.as_str(), None)
        )
    }

    pub async fn needs_transport_update(
        &self,
        store: &dyn CheckpointStore,
    ) -> Result<bool, IndexError> {
        let mut changed = false;
        for (stream, sequence) in self.transport_positions() {
            let key = Self::transport_key(stream);
            let saved = store.read_checkpoint(&key).await?;
            if let Some(saved) = &saved {
                transport_identity(&key, saved)
                    .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
            }
            changed |= saved
                .as_ref()
                .map_or(true, |saved| saved.sequence < sequence);
        }
        Ok(changed)
    }

    pub fn record_transport(&self, checkpoints: &mut BTreeMap<StreamId, u64>) {
        for (stream, sequence) in self.transport_positions() {
            let saved = checkpoints.entry(stream.clone()).or_insert(sequence);
            *saved = (*saved).max(sequence);
        }
    }

    fn transport_positions(&self) -> impl Iterator<Item = (&StreamId, u64)> + '_ {
        self.transport_sequence
            .into_iter()
            .map(|sequence| (&self.stream, sequence))
            .chain(self.positioned.iter().flat_map(|position| {
                position
                    .origin_transport
                    .iter()
                    .map(|(stream, sequence)| (stream, *sequence))
            }))
    }
}

pub(super) const INPUT_TRANSPORT_PREFIX: &str = "\0computation:input-transport:v1:";

pub(super) fn transport_identity(
    key: &str,
    checkpoint: &SourceCheckpoint,
) -> anyhow::Result<StreamId> {
    let key = key
        .strip_prefix(INPUT_TRANSPORT_PREFIX)
        .ok_or_else(|| anyhow::anyhow!("invalid input transport checkpoint"))?;
    let identity = super::query::progress_identity(key)?;
    anyhow::ensure!(
        checkpoint.source_position.is_none(),
        "invalid input transport checkpoint"
    );
    match identity {
        SourceProgressKey::Stream(stream) => Ok(stream),
        SourceProgressKey::Source(_) => anyhow::bail!("input transport checkpoint is not a stream"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validated_stateless_paths_restore_logical_producer_progress_and_both_transports() {
        let stream = StreamId::try_new("producer/out").expect("producer stream");
        let relay = StreamId::try_new("relay/out").expect("relay stream");
        for persistent in [false, true] {
            let mut original =
                GraphChangeCodec::encode_changes(&[], stream.clone(), 17, None).expect("input");
            let identity = GraphProducerIdentity::new(
                "scope".into(),
                "graph".into(),
                ComponentId::try_new("producer").expect("producer"),
                stream.clone(),
                persistent,
            )
            .expect("identity");
            GraphProducerProgress::annotate(&mut original, &identity, 3).expect("progress");
            let relayed = GraphChangeCodec::derive_changes(&original, &[], relay.clone(), 50)
                .expect("stateless relay");
            let owner = super::super::QuerySourceProgress::new(
                "graph",
                ComponentId::try_new("owner").expect("owner"),
            )
            .expect("progress owner");
            owner
                .bind_input_boundary(
                    identity.component_id(),
                    super::super::ComponentRole::Transformer,
                    None,
                    vec![stream.clone(), relay.clone()],
                )
                .expect("validated path");
            let input = GraphInputProgress::from_envelope(&relayed)
                .expect("transport progress")
                .with_consumer_boundary(&relayed, Some(&owner))
                .expect("producer progress");
            assert_eq!(input.sequence, 3);
            assert_eq!(input.identity, SourceProgressKey::Stream(stream.clone()));
            assert_eq!(
                input.key,
                super::super::query::progress_key(stream.as_str(), None)
            );
            assert_eq!(input.producer.as_ref(), persistent.then_some(&identity));
            assert_eq!(input.volatile_producer, !persistent);
            let mut transports = BTreeMap::new();
            input.record_transport(&mut transports);
            assert_eq!(transports[&stream], 17);
            assert_eq!(transports[&relay], 50);
        }
    }

    #[test]
    fn transaction_replay_uses_logical_source_progress_not_transport_sequence() {
        use crate::computation::v1::{SourceTransactionBuilder, SourceTransactionLimits};
        use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
        use std::num::{NonZeroU64, NonZeroUsize};
        for sequence in [1, 2] {
            let mut builder = SourceTransactionBuilder::new(
                "source",
                SourceTransactionLimits {
                    max_changes: NonZeroUsize::MIN,
                    max_bytes: NonZeroUsize::new(4096).expect("size"),
                    max_duration_ms: NonZeroU64::new(1000).expect("duration"),
                },
            )
            .expect("transaction");
            builder
                .push(SourceChange::Delete {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", "item"),
                        labels: Arc::from([]),
                        effective_from: 1000,
                    },
                })
                .expect("change");
            let envelope = builder
                .commit(
                    Bytes::from_static(b"transaction"),
                    Bytes::from_static(b"commit-position"),
                )
                .expect("commit")
                .into_replay_envelope(
                    StreamId::try_new("stream").expect("stream"),
                    42,
                    sequence,
                    chrono::DateTime::from_timestamp_millis(1000).expect("timestamp"),
                )
                .expect("replay");
            let progress = GraphInputProgress::from_envelope(&envelope).expect("runtime progress");
            assert_eq!(progress.sequence, 42);
            assert_eq!(
                progress.identity,
                SourceProgressKey::Source("source".into())
            );
            assert_eq!(
                progress.position.as_deref(),
                Some(b"commit-position".as_slice())
            );
            assert_eq!(envelope.system().sequence(), sequence);
        }
    }

    #[test]
    fn deriving_does_not_launder_an_explicit_volatile_producer() {
        let stream = StreamId::try_new("first/out").unwrap();
        let mut source = GraphChangeCodec::encode_changes(&[], stream.clone(), 1, None).unwrap();
        let identity = GraphProducerIdentity::volatile(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("first").unwrap(),
            stream,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut source, &identity, 1).unwrap();
        let output_stream = StreamId::try_new("second/out").unwrap();
        let derived =
            GraphChangeCodec::derive_changes(&source, &[], output_stream.clone(), 1).unwrap();
        let progress = GraphInputProgress::from_envelope(&derived).unwrap();
        assert!(progress.volatile_producer);
        assert_eq!(progress.identity, SourceProgressKey::Stream(output_stream));
        assert!(GraphProducerProgress::from_envelope(&derived)
            .unwrap()
            .is_none());
    }

    #[test]
    fn lineage_alone_does_not_invent_a_producer_durability_declaration() {
        let source =
            GraphChangeCodec::encode_changes(&[], StreamId::try_new("first/out").unwrap(), 1, None)
                .unwrap();
        let derived = GraphChangeCodec::derive_changes(
            &source,
            &[],
            StreamId::try_new("second/out").unwrap(),
            2,
        )
        .unwrap();
        let progress = GraphInputProgress::from_envelope(&derived).unwrap();
        assert!(!progress.volatile_producer);
        assert!(progress.producer.is_none());
        assert_eq!(progress.sequence, 2);
        assert_eq!(
            progress.identity,
            SourceProgressKey::Stream(StreamId::try_new("second/out").unwrap())
        );
    }
}
