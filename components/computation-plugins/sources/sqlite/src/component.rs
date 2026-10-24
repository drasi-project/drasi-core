// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ConsumerProgressView, GraphChangeCodec, InputPositionPolicy,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, SourceProgressView,
    SourceSchemaReader, SourceTransactionCodec, StreamId,
};
use drasi_computation_plugin_sdk::{
    NativeComponent, NativeConsumerProgress, NativeSource, NativeSourceProgress,
};
use drasi_core_types::interface::StorageDurability;
use drasi_sqlite_common::{
    native::{
        handle::Command,
        journal,
        progress::{Observed, Reader},
        runtime::{SqliteRuntime, StartContext},
        SqliteConfig, SqliteHandle, SqliteOutput,
    },
    RestApiConfig,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, RwLock};

pub(crate) enum Progress {
    None,
    Single(SourceProgressView),
    Shared {
        reader: ConsumerProgressView,
        members: Arc<[ComponentId]>,
    },
}

pub struct SqliteSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: SqliteConfig,
    commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
    runtime: SqliteRuntime,
    sequence: Arc<AtomicU64>,
    rest_api: Option<RestApiConfig>,
    progress: Progress,
    policy: Option<Arc<InputPositionPolicy>>,
    cleanup_required: bool,
    coordinator: Option<crate::coordinator::SourceBinding>,
}

impl SqliteSource {
    pub fn new(id: ComponentId, stream: StreamId, config: SqliteConfig) -> Result<Self> {
        anyhow::ensure!(
            config.replay.is_none(),
            "SQLite replay requires its actual progress binding"
        );
        Self::build(id, stream, config, Progress::None)
    }

    pub fn new_replayable(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: NativeSourceProgress,
    ) -> Result<Self> {
        Self::validate_replay(&config)?;
        Self::build(id, stream, config, Progress::Single(progress.reader()))
    }

    pub fn new_replayable_with_consumers(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: NativeConsumerProgress,
    ) -> Result<Self> {
        Self::validate_replay(&config)?;
        anyhow::ensure!(
            progress.reader().producer_id() == &id,
            "SQLite consumer progress belongs to another source"
        );
        Self::build(
            id,
            stream,
            config,
            Progress::Shared {
                reader: progress.reader(),
                members: progress.consumer_ids().to_vec().into(),
            },
        )
    }

    fn validate_replay(config: &SqliteConfig) -> Result<()> {
        anyhow::ensure!(
            config.replay.is_some()
                && config.path.is_some()
                && config.output == SqliteOutput::Transactions,
            "SQLite replay requires a file, journal bounds and complete-transaction output"
        );
        Ok(())
    }

    pub(crate) fn build(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: Progress,
    ) -> Result<Self> {
        config.validate()?;
        let commands = Arc::new(RwLock::new(None));
        let runtime = SqliteRuntime::new(commands.clone(), config.timeout());
        let policy = matches!(progress, Progress::Shared { .. })
            .then(|| journal::position_policy(&id))
            .transpose()?;
        Ok(Self {
            descriptor: Self::describe(id, config.output)?,
            stream,
            config,
            commands,
            runtime,
            progress,
            policy,
            sequence: Arc::new(AtomicU64::new(0)),
            rest_api: None,
            cleanup_required: false,
            coordinator: None,
        })
    }

    pub(crate) fn with_coordinator(mut self, binding: crate::coordinator::SourceBinding) -> Self {
        self.coordinator = Some(binding);
        self
    }

    pub fn describe(id: ComponentId, output: SqliteOutput) -> Result<ComponentDescriptor> {
        let schema = match output {
            SqliteOutput::Changes => GraphChangeCodec::schema(),
            SqliteOutput::Transactions => SourceTransactionCodec::schema(),
        };
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                schema.descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    pub fn handle(&self) -> SqliteHandle {
        SqliteHandle::new(self.commands.clone(), &self.config)
    }

    pub fn with_rest_api(mut self, config: RestApiConfig) -> Result<Self> {
        anyhow::ensure!(
            !self.cleanup_required && self.rest_api.is_none(),
            "native SQLite REST must be configured once before startup"
        );
        config.validate()?;
        self.rest_api = Some(config);
        Ok(self)
    }

    pub fn bound_rest_address(&self) -> Option<SocketAddr> {
        self.runtime.bound_rest_address()
    }
}

#[async_trait]
impl NativeComponent for SqliteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        let mut result = serde_json::json!({"stream":self.stream.as_str(), "settings":self.config});
        if let Some(rest) = &self.rest_api {
            result["rest_api"] = serde_json::to_value(rest)?;
        }
        if self.coordinator.is_some() {
            result["coordinated_snapshot"] = true.into();
        }
        Ok(result)
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        match &self.progress {
            Progress::Shared { reader, .. } => Some(reader.clone()),
            _ => None,
        }
    }
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        matches!(self.progress, Progress::Shared { .. })
            .then_some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        self.policy.clone()
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "native SQLite source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        if let Some(binding) = &self.coordinator {
            binding.ensure_active()?;
        }
        let mut progress = match &self.progress {
            Progress::None => None,
            Progress::Single(progress) => {
                let reader = progress.clone();
                let snapshot = reader.wait_ready().await?;
                anyhow::ensure!(
                    snapshot.persistent,
                    "native SQLite replay requires persistent consumer progress"
                );
                self.sequence.fetch_max(
                    snapshot
                        .transport_sequences
                        .get(&self.stream)
                        .copied()
                        .unwrap_or(0),
                    Ordering::Relaxed,
                );
                Some((Reader::Single(reader), Observed::Single(snapshot), None))
            }
            Progress::Shared { reader, members } => {
                let reader = reader.clone();
                let snapshot = reader.wait_ready().await?;
                anyhow::ensure!(
                    snapshot.consumers.keys().eq(members.iter()),
                    "SQLite consumer membership changed"
                );
                for (consumer, state) in &snapshot.consumers {
                    anyhow::ensure!(state.persistent && state.recovered,
                        "native SQLite replay requires persistent recovered progress for consumer {consumer}");
                    self.sequence.fetch_max(
                        state
                            .transport_sequences
                            .get(&self.stream)
                            .copied()
                            .unwrap_or(0),
                        Ordering::Relaxed,
                    );
                }
                Some((Reader::Group(reader), Observed::Group(snapshot), None))
            }
        };
        let mut lease = None;
        if let Some(binding) = &self.coordinator {
            binding.ensure_active()?;
            let progress = progress
                .as_mut()
                .context("coordinated SQLite source has no progress")?;
            let (epoch, exclusive) = binding.live_boundary(&progress.1)?;
            progress.2 = Some(epoch);
            lease = Some(exclusive);
        }
        let owner = self
            .coordinator
            .as_ref()
            .map(|binding| binding.owner.clone());
        self.runtime
            .start(
                StartContext {
                    source: self.descriptor.id().clone(),
                    stream: self.stream.clone(),
                    config: self.config.clone(),
                    sequence: self.sequence.clone(),
                    rest_api: self.rest_api.clone(),
                    progress,
                },
                Arc::new((owner, lease)),
            )
            .await?;
        if let Some(binding) = &self.coordinator {
            binding.ensure_active()?;
        }
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.runtime.stop().await?;
        self.cleanup_required = false;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for SqliteSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.runtime.source_schema_reader())
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        match &self.progress {
            Progress::Single(progress) => Some(progress.clone()),
            _ => None,
        }
    }
    fn replay_retention(&self) -> Option<(StorageDurability, ComponentId)> {
        self.recovery_reader().map(|reader| {
            (
                StorageDurability::LOCAL_PROCESS_RESTART,
                reader.component_id().clone(),
            )
        })
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        if let Some(binding) = &self.coordinator {
            binding.ensure_active()?;
        }
        let output = self.runtime.next().await?;
        if let Some(binding) = &self.coordinator {
            binding.ensure_active()?;
        }
        Ok(output)
    }
}
