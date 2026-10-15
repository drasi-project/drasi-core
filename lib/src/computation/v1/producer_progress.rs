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
    StreamId,
};

pub(super) const INPUT_OWNER_PREFIX: &str = "computation:input-owner:";

use drasi_computation_contracts::has_distinct_producer_lineage;
pub use drasi_computation_contracts::{GraphProducerIdentity, GraphProducerProgress};

pub(super) struct GraphInputProgress {
    stream: StreamId,
    transport_sequence: Option<u64>,
    pub key: String,
    pub identity: SourceProgressKey,
    pub sequence: u64,
    pub position: Option<Bytes>,
    pub producer: Option<GraphProducerIdentity>,
    volatile_producer: bool,
}

impl GraphInputProgress {
    pub(super) fn from_stream(input: &ChangeEnvelope, sequence: u64) -> Self {
        let stream = input.system().stream().clone();
        Self {
            key: super::query::progress_key(stream.as_str(), None),
            identity: SourceProgressKey::Stream(stream.clone()),
            stream,
            transport_sequence: None,
            sequence,
            position: input.system().source_position().cloned(),
            producer: None,
            volatile_producer: false,
        }
    }

    pub fn from_envelope(input: &ChangeEnvelope) -> anyhow::Result<Self> {
        let (producer, inherited_volatile) = GraphProducerProgress::read_scoped(input, false)?;
        if let Some(progress) = producer.as_ref() {
            return Ok(Self {
                stream: input.system().stream().clone(),
                transport_sequence: None,
                key: super::query::progress_key(input.system().stream().as_str(), None),
                identity: SourceProgressKey::Stream(input.system().stream().clone()),
                sequence: progress.sequence(),
                position: None,
                producer: progress
                    .identity()
                    .persistent()
                    .then(|| progress.identity().clone()),
                volatile_producer: !progress.identity().persistent(),
            });
        }
        if has_distinct_producer_lineage(input) {
            let mut progress = Self::from_stream(input, input.system().sequence());
            // Inherited source metadata is causal history, not this producer's cursor.
            progress.position = None;
            // Lineage alone does not declare durability; an explicit volatile
            // ancestor still cannot become a durable boundary by derivation.
            progress.volatile_producer = inherited_volatile;
            return Ok(progress);
        }
        if let Some((source, sequence)) = super::SourceTransactionCodec::replay_progress(input)? {
            return Ok(Self {
                stream: input.system().stream().clone(),
                transport_sequence: Some(input.system().sequence()),
                key: super::query::progress_key(input.system().stream().as_str(), Some(&source)),
                identity: SourceProgressKey::Source(source),
                sequence,
                position: input.system().source_position().cloned(),
                producer: None,
                volatile_producer: false,
            });
        }
        let raw = GraphChangeCodec::source_metadata(input)?;
        let stable = raw.as_ref().and_then(|metadata| {
            metadata
                .sequence
                .map(|sequence| (metadata.source_id.as_str(), sequence))
        });
        Ok(Self {
            stream: input.system().stream().clone(),
            transport_sequence: stable.map(|_| input.system().sequence()),
            key: super::query::progress_key(
                input.system().stream().as_str(),
                stable.map(|(source, _)| source),
            ),
            identity: match stable {
                Some((source, _)) => SourceProgressKey::Source(source.to_owned()),
                None => SourceProgressKey::Stream(input.system().stream().clone()),
            },
            sequence: stable.map_or(input.system().sequence(), |(_, sequence)| sequence),
            position: raw
                .as_ref()
                .and_then(|metadata| metadata.source_position.as_ref())
                .map(|position| Bytes::copy_from_slice(position))
                .or_else(|| input.system().source_position().cloned()),
            producer: None,
            volatile_producer: false,
        })
    }

    fn owner_key(&self) -> String {
        format!(
            "{INPUT_OWNER_PREFIX}{}",
            super::query::progress_key(self.stream.as_str(), None)
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
        if self.needs_transport_update(store).await? {
            store
                .stage_checkpoint(
                    &self.transport_key(),
                    self.transport_sequence.expect("transport update"),
                    None,
                )
                .await?;
        }
        Ok(())
    }

    fn transport_key(&self) -> String {
        format!(
            "{INPUT_TRANSPORT_PREFIX}{}",
            super::query::progress_key(self.stream.as_str(), None)
        )
    }

    pub async fn needs_transport_update(
        &self,
        store: &dyn CheckpointStore,
    ) -> Result<bool, IndexError> {
        let Some(sequence) = self.transport_sequence else {
            return Ok(false);
        };
        let key = self.transport_key();
        let saved = store.read_checkpoint(&key).await?;
        if let Some(saved) = &saved {
            transport_identity(&key, saved)
                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        }
        Ok(saved
            .as_ref()
            .map_or(true, |saved| saved.sequence < sequence))
    }

    pub fn record_transport(&self, checkpoints: &mut BTreeMap<StreamId, u64>) {
        if let Some(sequence) = self.transport_sequence {
            let saved = checkpoints.entry(self.stream.clone()).or_insert(sequence);
            *saved = (*saved).max(sequence);
        }
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
