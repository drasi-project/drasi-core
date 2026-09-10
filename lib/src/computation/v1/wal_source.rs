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

use super::*;
use crate::{
    channels::{SourceEvent, SourceEventWrapper},
    wal::WalProvider,
};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::models::SourceChange;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

/// A reference to an explicitly registered WAL partition. The graph never
/// deletes, rewinds or re-registers an unrelated legacy Source's WAL.
pub struct WalSourceResource {
    pub provider: Arc<dyn WalProvider>,
    pub partition: String,
}

pub struct WalReplaySource {
    descriptor: ComponentDescriptor,
    resource: Arc<WalSourceResource>,
    stream: StreamId,
    resume: u64,
    cursor: u64,
    producer_sequence: u64,
    pending: VecDeque<(u64, SourceChange)>,
    progress: Option<Arc<QuerySourceProgress>>,
}

fn descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("out").expect("port"),
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("WAL source descriptor")
}

impl WalReplaySource {
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        resource: Arc<WalSourceResource>,
        resume_after: u64,
    ) -> Self {
        Self {
            descriptor: descriptor(id),
            resource,
            stream,
            resume: resume_after,
            cursor: resume_after,
            producer_sequence: 0,
            pending: VecDeque::new(),
            progress: None,
        }
    }
    pub fn with_source_progress(mut self, progress: Arc<QuerySourceProgress>) -> Self {
        self.progress = Some(progress);
        self
    }
    async fn read(&mut self) -> anyhow::Result<()> {
        let next = self
            .cursor
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("WAL resume sequence exhausted"))?;
        let entries = self
            .resource
            .provider
            .read_from(&self.resource.partition, next)
            .await?;
        let mut previous = self.cursor;
        for (sequence, event) in entries {
            if sequence <= previous || matches!(event, SourceChange::Future { .. }) {
                anyhow::bail!("WAL returned unordered or unsupported source data");
            }
            previous = sequence;
            self.pending.push_back((sequence, event));
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for WalReplaySource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.pending.clear();
        self.cursor = self.resume;
        if let Some(progress) = &self.progress {
            let snapshot = progress.wait_ready().await?;
            if !snapshot.persistent {
                anyhow::bail!("WAL resume requires persistent committed query progress");
            }
            if let Some(checkpoint) = snapshot
                .checkpoints
                .get(&SourceProgressKey::Source(self.resource.partition.clone()))
            {
                self.cursor = self.cursor.max(checkpoint.sequence);
            }
        }
        self.read().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.pending.clear();
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for WalReplaySource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        loop {
            if let Some((sequence, change)) = self.pending.pop_front() {
                let timestamp = chrono::DateTime::from_timestamp_millis(i64::try_from(
                    change.get_transaction_time(),
                )?)
                .ok_or_else(|| anyhow::anyhow!("WAL event time cannot be represented"))?;
                let event = Arc::new(SourceEventWrapper {
                    source_id: self.resource.partition.clone(),
                    event: SourceEvent::Change(change),
                    timestamp,
                    sequence: Some(sequence),
                    source_position: Some(Bytes::copy_from_slice(&sequence.to_be_bytes())),
                    profiling: None,
                });
                let producer_sequence = self
                    .producer_sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("WAL adapter sequence exhausted"))?;
                let envelope = GraphChangeCodec::encode_source_event(
                    event,
                    self.descriptor.id(),
                    self.stream.clone(),
                    producer_sequence,
                    None,
                )?;
                self.cursor = sequence;
                self.producer_sequence = producer_sequence;
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope,
                }));
            }
            self.read().await?;
            if self.pending.is_empty() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

pub struct WalReplaySourceFactory {
    descriptor: FactoryDescriptor,
}

impl Default for WalReplaySourceFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/wal-replay-source", "1")
                    .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([
                        (
                            Arc::from("stream"),
                            ConfigurationField {
                                value_type: ConfigurationType::String,
                                required: true,
                                secret: false,
                            },
                        ),
                        (
                            Arc::from("resume_after"),
                            ConfigurationField {
                                value_type: ConfigurationType::Integer,
                                required: false,
                                secret: false,
                            },
                        ),
                    ]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([
                    (
                        Arc::from("wal"),
                        ResourceRequirement::exactly_one::<WalSourceResource>(ResourceRole::Wal),
                    ),
                    (
                        Arc::from("source_progress"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                                ResourceRole::Checkpoint,
                            )
                        },
                    ),
                ]),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for WalReplaySourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate_scope(
        &self,
        graph_id: &str,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate_resources(spec, declarations, resources)?;
        for id in spec
            .dependencies
            .get("source_progress")
            .into_iter()
            .flatten()
        {
            if let Some(handle) = resources.get(id) {
                if handle.get::<QuerySourceProgressResource>()?.0.graph_id() != graph_id {
                    anyhow::bail!("source progress belongs to another graph");
                }
            }
        }
        Ok(())
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        if spec.descriptor != descriptor(spec.descriptor.id().clone()) {
            anyhow::bail!("invalid WAL source ports/schema");
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("resume_after") {
            if value.as_u64().is_none() {
                anyhow::bail!("WAL resume position must be unsigned");
            }
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid WAL stream"))?,
            )?;
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let wal = context
            .resources::<WalSourceResource>("wal")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing WAL resource"))
            })?;
        let stream = context
            .configuration()
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing WAL stream"))
            })?;
        let resume = match context.configuration().get("resume_after") {
            None => 0,
            Some(value) => value.as_u64().ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid WAL resume sequence"))
            })?,
        };
        let mut source = WalReplaySource::new(
            context.component_id.clone(),
            StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?,
            wal,
            resume,
        );
        if context
            .specification
            .dependencies
            .contains_key("source_progress")
        {
            if let Some(progress) = context
                .resources::<QuerySourceProgressResource>("source_progress")
                .map_err(ComponentCreationError::terminal)?
                .pop()
            {
                if progress.0.graph_id() != context.graph_id.as_ref() {
                    return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                        "source progress belongs to another graph"
                    )));
                }
                source = source.with_source_progress(progress.0.clone());
            }
        }
        Ok(ConstructedComponent::source(Box::new(source)))
    }
}
