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

use std::sync::Arc;

use bytes::Bytes;
use drasi_core::interface::{CheckpointStore, IndexError, SourceCheckpoint};
use serde::{Deserialize, Serialize};

use super::{
    ChangeEnvelope, ComponentId, ContextEntry, ContextValue, GraphChangeCodec, SourceProgressKey,
    StreamId,
};

const PROGRESS: &str = "drasi.graph-producer-progress.v1";
pub(super) const INPUT_OWNER_PREFIX: &str = "computation:input-owner:";

/// The owner of a graph-change producer's logical output sequence.
///
/// A durable incarnation is persisted with its configuration, not recreated at
/// process startup. Transport emissions may replay the same logical sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphProducerIdentity {
    construction_scope: String,
    graph_id: String,
    component_id: ComponentId,
    stream: StreamId,
    incarnation: uuid::Uuid,
    persistent: bool,
}

impl GraphProducerIdentity {
    pub(super) fn new(
        scope: String,
        graph_id: String,
        component_id: ComponentId,
        stream: StreamId,
        persistent: bool,
    ) -> anyhow::Result<Self> {
        let identity = Self {
            construction_scope: scope,
            graph_id,
            component_id,
            stream,
            incarnation: uuid::Uuid::new_v4(),
            persistent,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn construction_scope(&self) -> &str {
        &self.construction_scope
    }
    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }
    pub fn incarnation(&self) -> uuid::Uuid {
        self.incarnation
    }
    pub fn persistent(&self) -> bool {
        self.persistent
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        super::data::validate_identifier("producer construction scope", &self.construction_scope)?;
        super::data::validate_identifier("producer graph", &self.graph_id)?;
        anyhow::ensure!(!self.incarnation.is_nil(), "nil graph producer incarnation");
        Ok(())
    }
}

/// Versioned graph-only progress, separate from raw source provenance and the
/// strictly increasing transport emission sequence. Does not change legacy FFI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphProducerProgress {
    version: u32,
    identity: GraphProducerIdentity,
    sequence: u64,
}

impl GraphProducerProgress {
    pub fn identity(&self) -> &GraphProducerIdentity {
        &self.identity
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(super) fn annotate(
        envelope: &mut ChangeEnvelope,
        identity: &GraphProducerIdentity,
        sequence: u64,
    ) -> anyhow::Result<()> {
        identity.validate()?;
        anyhow::ensure!(
            sequence > 0 && identity.stream == *envelope.system().stream(),
            "invalid graph producer logical progress"
        );
        envelope.append_annotation(ContextEntry::try_new(
            identity.component_id.clone(),
            PROGRESS,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&Self {
                version: 1,
                identity: identity.clone(),
                sequence,
            })?)),
        )?)?;
        Ok(())
    }

    pub fn from_envelope(envelope: &ChangeEnvelope) -> anyhow::Result<Option<Self>> {
        Self::read(envelope, false)
    }

    /// Query results retain input annotations as history. Only a marker for the
    /// current stream supersedes the query's own recovery identity.
    pub(super) fn from_query_envelope(envelope: &ChangeEnvelope) -> anyhow::Result<Option<Self>> {
        Self::read(envelope, true)
    }

    fn read(envelope: &ChangeEnvelope, allow_query_ancestor: bool) -> anyhow::Result<Option<Self>> {
        let Some(entry) = envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == PROGRESS)
        else {
            return Ok(None);
        };
        let ContextValue::Bytes(bytes) = entry.value() else {
            anyhow::bail!("graph producer progress is not bytes");
        };
        let progress: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            progress.version == 1,
            "unsupported graph producer progress version"
        );
        progress.identity.validate()?;
        anyhow::ensure!(
            progress.sequence > 0 && progress.identity.component_id.as_str() == entry.contributor(),
            "graph producer progress does not identify the immediate producer"
        );
        if allow_query_ancestor && progress.identity.stream != *envelope.system().stream() {
            return Ok(None);
        }
        anyhow::ensure!(
            progress.identity.stream == *envelope.system().stream(),
            "graph producer progress does not identify the immediate producer"
        );
        Ok(Some(progress))
    }
}

pub(super) struct GraphInputProgress {
    stream: StreamId,
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
            sequence,
            position: input.system().source_position().cloned(),
            producer: None,
            volatile_producer: false,
        }
    }

    pub fn from_envelope(input: &ChangeEnvelope) -> anyhow::Result<Self> {
        let producer = GraphProducerProgress::from_envelope(input)?;
        if let Some(progress) = producer
            .as_ref()
            .filter(|progress| progress.identity.persistent)
        {
            return Ok(Self {
                stream: input.system().stream().clone(),
                key: super::query::progress_key(input.system().stream().as_str(), None),
                identity: SourceProgressKey::Stream(input.system().stream().clone()),
                sequence: progress.sequence,
                position: None,
                producer: Some(progress.identity.clone()),
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
            // Volatile pipelines keep their existing source-progress semantics,
            // but cannot be mistaken for a durable recovery boundary.
            volatile_producer: producer.is_some(),
        })
    }

    fn owner_key(&self) -> String {
        format!(
            "{INPUT_OWNER_PREFIX}{}",
            super::query::progress_key(self.stream.as_str(), None)
        )
    }

    pub async fn validate(
        &self,
        store: &dyn CheckpointStore,
        persistent: bool,
    ) -> anyhow::Result<Option<SourceCheckpoint>> {
        anyhow::ensure!(
            !persistent || !self.volatile_producer,
            "persistent graph processing cannot recover through volatile middleware"
        );
        let saved = store.read_checkpoint(&self.key).await?;
        let owner = store.read_checkpoint(&self.owner_key()).await?;
        match (&self.producer, owner) {
            (Some(identity), owner) => {
                anyhow::ensure!(
                    !persistent || identity.persistent,
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
            .await
    }
}
