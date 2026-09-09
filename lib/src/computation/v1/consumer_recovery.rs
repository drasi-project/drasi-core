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
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{
    ComponentDescriptor, ComponentId, ComputationComponent, EnvelopeSink, InputEnvelope,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec,
    QueryHistoryError, QueryResults, SinkCompletion, StreamId, SystemMetadata, Transformer,
    WakeupSource,
};

#[async_trait]
pub trait ConsumerProgressStore: Send + Sync {
    async fn load(&self, query: &str) -> anyhow::Result<Option<ConsumerCheckpoint>>;
    async fn commit_handled(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()>;
    async fn allocate_sequence(&self, stream: &StreamId) -> anyhow::Result<u64>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsumerCheckpoint {
    pub sequence: u64,
    pub generation: u64,
}

#[derive(Default)]
pub struct MemoryConsumerProgress {
    handled: Mutex<HashMap<String, ConsumerCheckpoint>>,
    sequences: Mutex<HashMap<String, u64>>,
}

#[async_trait]
impl ConsumerProgressStore for MemoryConsumerProgress {
    async fn load(&self, query: &str) -> anyhow::Result<Option<ConsumerCheckpoint>> {
        Ok(self.handled.lock().await.get(query).copied())
    }
    async fn commit_handled(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        let mut handled = self.handled.lock().await;
        if handled.get(query).is_some_and(|previous| {
            previous.sequence > checkpoint.sequence || previous.generation > checkpoint.generation
        }) {
            anyhow::bail!("consumer checkpoint regression");
        }
        handled.insert(query.to_owned(), checkpoint);
        Ok(())
    }
    async fn allocate_sequence(&self, stream: &StreamId) -> anyhow::Result<u64> {
        let mut sequences = self.sequences.lock().await;
        let previous = sequences.get(stream.as_str()).copied().unwrap_or(0);
        let next = previous
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("replay producer sequence exhausted"))?;
        sequences.insert(stream.to_string(), next);
        Ok(next)
    }
}

/// A graph/consumer-scoped view over an explicitly supplied state store.
/// Handler side effects and checkpoint persistence are not one external transaction.
pub struct StateStoreConsumerProgress {
    provider: Arc<dyn crate::state_store::StateStoreProvider>,
    partition: String,
    lock: Mutex<()>,
}

impl StateStoreConsumerProgress {
    pub fn new(
        graph: &str,
        consumer: &str,
        provider: Arc<dyn crate::state_store::StateStoreProvider>,
    ) -> anyhow::Result<Self> {
        super::data::validate_identifier("graph", graph)?;
        super::data::validate_identifier("consumer", consumer)?;
        let encode = |value: &str| {
            value
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        Ok(Self {
            provider,
            partition: format!(
                "computation-consumer:{}:{}",
                encode(graph),
                encode(consumer)
            ),
            lock: Mutex::new(()),
        })
    }
    async fn read(&self, key: &str) -> anyhow::Result<Option<u64>> {
        self.provider
            .get(&self.partition, key)
            .await?
            .map(|bytes| {
                Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("invalid consumer progress record")
                })?))
            })
            .transpose()
    }
}

#[async_trait]
impl ConsumerProgressStore for StateStoreConsumerProgress {
    async fn load(&self, query: &str) -> anyhow::Result<Option<ConsumerCheckpoint>> {
        self.provider
            .get(&self.partition, &format!("handled:{query}"))
            .await?
            .map(|bytes| {
                if bytes.len() != 16 {
                    anyhow::bail!("invalid consumer checkpoint record");
                }
                Ok(ConsumerCheckpoint {
                    sequence: u64::from_be_bytes(bytes[..8].try_into()?),
                    generation: u64::from_be_bytes(bytes[8..].try_into()?),
                })
            })
            .transpose()
    }
    async fn commit_handled(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        let _lock = self.lock.lock().await;
        let key = format!("handled:{query}");
        if self.load(query).await?.is_some_and(|old| {
            old.sequence > checkpoint.sequence || old.generation > checkpoint.generation
        }) {
            anyhow::bail!("consumer checkpoint regression");
        }
        let mut bytes = checkpoint.sequence.to_be_bytes().to_vec();
        bytes.extend_from_slice(&checkpoint.generation.to_be_bytes());
        self.provider.set(&self.partition, &key, bytes).await?;
        Ok(())
    }
    async fn allocate_sequence(&self, stream: &StreamId) -> anyhow::Result<u64> {
        let _lock = self.lock.lock().await;
        let key = format!("producer:{stream}");
        let sequence = self
            .read(&key)
            .await?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("replay sequence exhausted"))?;
        self.provider
            .set(&self.partition, &key, sequence.to_be_bytes().to_vec())
            .await?;
        Ok(sequence)
    }
}

pub struct ConsumerProgressResource(pub Arc<dyn ConsumerProgressStore>);
pub struct QueryResultsResource(pub QueryResults);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerRecoveryPolicy {
    Strict,
    AutoReset,
    AutoSkipGap,
}

struct ReplayWakeup {
    results: QueryResults,
    pending: Arc<AtomicBool>,
}
#[async_trait]
impl WakeupSource for ReplayWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        if !self.pending.load(Ordering::Acquire) {
            return std::future::pending().await;
        }
        self.results.wait_ready().await?;
        Ok(())
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.pending.load(Ordering::Acquire))
    }
}

/// A query-row transformer that joins an initial snapshot/retained suffix to the
/// already bound live pipe. Consumer checkpoints are written only by actual sinks.
pub struct QueryReplayTransformer {
    descriptor: ComponentDescriptor,
    query_id: String,
    results: QueryResults,
    progress: Arc<dyn ConsumerProgressStore>,
    policy: ConsumerRecoveryPolicy,
    stream: StreamId,
    pending: Arc<AtomicBool>,
    seen: u64,
}

impl QueryReplayTransformer {
    pub fn new(
        id: ComponentId,
        query_id: String,
        stream: StreamId,
        results: QueryResults,
        progress: Arc<dyn ConsumerProgressStore>,
        policy: ConsumerRecoveryPolicy,
    ) -> Self {
        let descriptor = ComponentDescriptor::try_new(
            id,
            vec![
                PortDescriptor::new(
                    PortId::try_new("in").expect("port"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                ),
                PortDescriptor::new(
                    PortId::try_new("out").expect("port"),
                    PortDirection::Output,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                ),
            ],
        )
        .expect("replay descriptor");
        Self {
            descriptor,
            query_id,
            results,
            progress,
            policy,
            stream,
            pending: Arc::new(AtomicBool::new(true)),
            seen: 0,
        }
    }

    async fn forward(&self, envelope: &super::ChangeEnvelope) -> anyhow::Result<OutputEnvelope> {
        let sequence = self.progress.allocate_sequence(&self.stream).await?;
        Ok(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: envelope.derive(
                super::emission_id(&self.stream, sequence)?,
                envelope.changes().clone(),
                SystemMetadata::new(self.stream.clone(), sequence).with_timestamp(
                    envelope
                        .system()
                        .timestamp()
                        .unwrap_or_else(chrono::Utc::now),
                ),
            ),
        })
    }

    async fn initial(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.pending.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        self.results.wait_ready().await?;
        let checkpoint = self.progress.load(&self.query_id).await?;
        let snapshot = self.results.snapshot()?;
        let mut reset = checkpoint.is_none();
        let mut replay = Vec::new();
        if let Some(checkpoint) = checkpoint {
            let after = checkpoint.sequence;
            if after > snapshot.as_of_sequence || checkpoint.generation > snapshot.generation {
                anyhow::bail!("consumer checkpoint is ahead of the current query state");
            }
            if checkpoint.generation != snapshot.generation {
                match self.policy {
                    ConsumerRecoveryPolicy::Strict => anyhow::bail!(
                        "query reset generation changed; explicit snapshot recovery is required"
                    ),
                    ConsumerRecoveryPolicy::AutoReset => reset = true,
                    ConsumerRecoveryPolicy::AutoSkipGap => {
                        log::warn!("Consumer explicitly skips a query reset generation");
                        self.seen = snapshot.as_of_sequence;
                        self.pending.store(false, Ordering::Release);
                        return Ok(Vec::new());
                    }
                }
            }
            if !reset {
                match self.results.replay(after) {
                    Ok(history) => replay = history,
                    Err(QueryHistoryError::Unavailable { oldest, latest, .. })
                        if after <= latest =>
                    {
                        match self.policy {
                            ConsumerRecoveryPolicy::Strict => {
                                return Err(QueryHistoryError::Unavailable {
                                    requested: after,
                                    oldest,
                                    latest,
                                }
                                .into())
                            }
                            ConsumerRecoveryPolicy::AutoReset => reset = true,
                            ConsumerRecoveryPolicy::AutoSkipGap => {
                                log::warn!(
                                    "Consumer {} explicitly skips query history gap after {after}",
                                    self.descriptor.id()
                                );
                                replay = match self.results.replay(oldest.saturating_sub(1)) {
                                    Ok(history) => history,
                                    Err(QueryHistoryError::Unavailable {
                                        oldest, latest, ..
                                    }) if oldest == latest => Vec::new(),
                                    Err(error) => return Err(error.into()),
                                };
                            }
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        let mut output = Vec::new();
        if reset {
            let sequence = self.progress.allocate_sequence(&self.stream).await?;
            output.push(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: QueryChangeCodec::snapshot_envelope(
                    &self.query_id,
                    &snapshot,
                    self.descriptor.id(),
                    SystemMetadata::new(self.stream.clone(), sequence)
                        .with_timestamp(chrono::Utc::now()),
                )?,
            });
            self.seen = snapshot.as_of_sequence;
        } else {
            self.seen = checkpoint
                .map(|checkpoint| checkpoint.sequence)
                .unwrap_or(0);
            for envelope in replay {
                self.seen = self.seen.max(QueryChangeCodec::query_sequence(&envelope)?);
                output.push(self.forward(&envelope).await?);
            }
            if self.policy == ConsumerRecoveryPolicy::AutoSkipGap {
                self.seen = self.seen.max(snapshot.as_of_sequence);
            }
        }
        self.pending.store(false, Ordering::Release);
        Ok(output)
    }
}

#[async_trait]
impl ComputationComponent for QueryReplayTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.pending.store(true, Ordering::Release);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for QueryReplayTransformer {
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(Arc::new(ReplayWakeup {
            results: self.results.clone(),
            pending: self.pending.clone(),
        }))
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.initial().await
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let mut output = self.initial().await?;
        if QueryChangeCodec::metadata(&input.envelope)?.query_id != self.query_id {
            anyhow::bail!("live query output identity mismatch");
        }
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        if sequence > self.seen {
            output.push(self.forward(&input.envelope).await?);
            self.seen = sequence;
        }
        Ok(output)
    }
}

/// Native handling checkpoint adapter. Acceptance-only legacy queues are rejected.
pub struct CheckpointedSink {
    inner: Box<dyn EnvelopeSink>,
    progress: Arc<dyn ConsumerProgressStore>,
    accept_skipped_resets: bool,
}

impl CheckpointedSink {
    pub fn new(
        inner: Box<dyn EnvelopeSink>,
        progress: Arc<dyn ConsumerProgressStore>,
    ) -> anyhow::Result<Self> {
        if inner.completion() != SinkCompletion::Handled {
            anyhow::bail!("handled checkpoints cannot wrap an acceptance-only sink");
        }
        Ok(Self {
            inner,
            progress,
            accept_skipped_resets: false,
        })
    }

    /// Explicitly accept potentially stale external state after an upstream
    /// AutoSkipGap decision instead of requiring a replacement snapshot.
    pub fn allow_skipped_resets(mut self) -> Self {
        self.accept_skipped_resets = true;
        self
    }
}

#[async_trait]
impl ComputationComponent for CheckpointedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.inner.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl EnvelopeSink for CheckpointedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn supports_snapshot(&self) -> bool {
        self.inner.supports_snapshot()
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let query = QueryChangeCodec::metadata(&input.envelope)?.query_id;
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        let generation = QueryChangeCodec::query_generation(&input.envelope)?;
        if let Some(handled) = self.progress.load(&query).await? {
            if generation < handled.generation
                || (generation > handled.generation && sequence < handled.sequence)
            {
                anyhow::bail!("stale query generation or checkpoint regression");
            }
            if handled.generation == generation && handled.sequence >= sequence {
                return Ok(());
            }
            if generation != handled.generation
                && !QueryChangeCodec::is_snapshot(&input.envelope)
                && !self.accept_skipped_resets
            {
                anyhow::bail!(
                    "query reset requires a replacement snapshot or explicit skip policy"
                );
            }
        }
        if QueryChangeCodec::is_snapshot(&input.envelope) {
            if !self.inner.supports_snapshot() {
                anyhow::bail!("sink does not support explicit snapshot replacement");
            }
            self.inner.replace_snapshot(input).await?;
        } else {
            self.inner.handle(input).await?;
        }
        self.progress
            .commit_handled(
                &query,
                ConsumerCheckpoint {
                    sequence,
                    generation,
                },
            )
            .await
    }
}

pub struct QueryReplayFactory {
    descriptor: super::FactoryDescriptor,
}

impl Default for QueryReplayFactory {
    fn default() -> Self {
        Self {
            descriptor: super::FactoryDescriptor {
                implementation: super::ImplementationIdentity::try_new("drasi/query-replay", "1")
                    .expect("implementation"),
                role: super::ComponentRole::Transformer,
                configuration_version: 1,
                configuration: super::ConfigurationSchema {
                    fields: [("query_id", true), ("stream", true), ("recovery", false)]
                        .into_iter()
                        .map(|(name, required)| {
                            (
                                Arc::from(name),
                                super::ConfigurationField {
                                    value_type: super::ConfigurationType::String,
                                    required,
                                    secret: false,
                                },
                            )
                        })
                        .collect(),
                    allow_additional: false,
                },
                dependencies: std::collections::BTreeMap::from([
                    (
                        Arc::from("results"),
                        super::ResourceRequirement::exactly_one::<QueryResultsResource>(
                            super::ResourceRole::LiveResults,
                        ),
                    ),
                    (
                        Arc::from("progress"),
                        super::ResourceRequirement::exactly_one::<ConsumerProgressResource>(
                            super::ResourceRole::StateStore,
                        ),
                    ),
                ]),
            },
        }
    }
}

#[async_trait]
impl super::ComponentFactory for QueryReplayFactory {
    fn descriptor(&self) -> &super::FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &super::ComponentSpecification) -> anyhow::Result<()> {
        for port in spec.descriptor.ports() {
            super::data::validate_schema(QueryChangeCodec::schema().descriptor(), port.schema())?;
        }
        if spec.descriptor.ports().len() != 2
            || !spec
                .descriptor
                .ports()
                .iter()
                .any(|port| port.id().as_str() == "in" && port.direction() == PortDirection::Input)
            || !spec.descriptor.ports().iter().any(|port| {
                port.id().as_str() == "out" && port.direction() == PortDirection::Output
            })
        {
            anyhow::bail!("query replay requires one input and one output");
        }
        if let Some(super::ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid replay stream"))?,
            )?;
        }
        if let Some(super::ConfigurationValue::Literal(value)) = spec.configuration.get("recovery")
        {
            if !matches!(
                value.as_str(),
                Some("strict" | "auto_reset" | "auto_skip_gap")
            ) {
                anyhow::bail!("unknown consumer recovery policy");
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: super::ConstructionContext,
    ) -> Result<super::ConstructedComponent, super::ComponentCreationError> {
        let results = context
            .resources::<QueryResultsResource>("results")
            .map_err(super::ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!("missing query results"))
            })?;
        let progress = context
            .resources::<ConsumerProgressResource>("progress")
            .map_err(super::ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!(
                    "missing consumer progress"
                ))
            })?;
        let config = context.configuration();
        let query = config
            .get("query_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!("missing query id"))
            })?;
        let stream = config
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!("missing stream"))
            })?;
        let policy = match config
            .get("recovery")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("strict")
        {
            "strict" => ConsumerRecoveryPolicy::Strict,
            "auto_reset" => ConsumerRecoveryPolicy::AutoReset,
            "auto_skip_gap" => ConsumerRecoveryPolicy::AutoSkipGap,
            _ => {
                return Err(super::ComponentCreationError::terminal(anyhow::anyhow!(
                    "unknown consumer recovery policy"
                )))
            }
        };
        let instance = QueryReplayTransformer::new(
            context.component_id.clone(),
            query.to_owned(),
            StreamId::try_new(stream).map_err(super::ComponentCreationError::terminal)?,
            results.0.clone(),
            progress.0.clone(),
            policy,
        );
        Ok(super::ConstructedComponent::transformer(Box::new(instance)))
    }
}
