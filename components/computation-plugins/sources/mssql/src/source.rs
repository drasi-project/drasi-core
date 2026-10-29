// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{Configuration, MsSqlConfig};
mod processing;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    exchange::{SourceEvent, SourceEventWrapper},
    ComponentDescriptor, ComponentId, ConsumerProgressSnapshot, ConsumerProgressView,
    GraphChangeCodec, InputPositionPolicy, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SourceProgressKey, SourceSchemaReader, StreamId,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};
use drasi_core_types::{interface::StorageDurability, schema::SourceSchema};
use drasi_mssql_common::native::{self, Cursor, CursorComparator, Poll, Work};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc,
    },
};

struct SchemaReader {
    config: MsSqlConfig,
    generation: Arc<AtomicU64>,
}
#[async_trait]
impl SourceSchemaReader for SchemaReader {
    async fn describe_schema(&self) -> Result<Option<SourceSchema>> {
        let generation = self.generation.load(AtomicOrdering::Acquire);
        ensure!(
            generation % 2 == 1,
            "SQL Server source schema generation is retired"
        );
        let schema =
            tokio::time::timeout(self.config.timeout(), native::describe_schema(&self.config))
                .await
                .context("SQL Server catalog read timed out")??;
        ensure!(
            self.generation.load(AtomicOrdering::Acquire) == generation,
            "SQL Server schema read belongs to a retired generation"
        );
        Ok(Some(schema))
    }
}

pub struct MsSqlSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: MsSqlConfig,
    progress: ConsumerProgressView,
    members: BTreeSet<ComponentId>,
    policy: Arc<InputPositionPolicy>,
    generation: Arc<AtomicU64>,
    schema_reader: Arc<SchemaReader>,
    work: Work<Poll>,
    queue: VecDeque<native::Change>,
    cursor: Option<Cursor>,
    origin: Option<Cursor>,
    observed: BTreeMap<ComponentId, (u64, Option<Cursor>)>,
    initialized: bool,
    cleanup: bool,
    sequence: u64,
}

impl MsSqlSource {
    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: MsSqlConfig,
        progress: ConsumerProgressView,
        members: &[ComponentId],
    ) -> Result<Self> {
        config.validate()?;
        let unique: BTreeSet<_> = members.iter().cloned().collect();
        ensure!(
            progress.producer_id() == &id
                && !unique.is_empty()
                && unique.len() == members.len()
                && !unique.contains(&id),
            "invalid SQL Server consumer membership"
        );
        let generation = Arc::new(AtomicU64::new(0));
        let schema_reader = Arc::new(SchemaReader {
            config: config.clone(),
            generation: generation.clone(),
        });
        Ok(Self {
            policy: Arc::new(InputPositionPolicy::new(
                SourceProgressKey::Source(id.to_string()),
                Arc::new(CursorComparator),
            )?),
            descriptor: Self::describe(id)?,
            stream,
            config,
            progress,
            members: unique,
            generation,
            schema_reader,
            work: Work::default(),
            queue: VecDeque::new(),
            cursor: None,
            origin: None,
            observed: BTreeMap::new(),
            initialized: false,
            cleanup: false,
            sequence: 0,
        })
    }
    fn observe(&mut self, snapshot: &ConsumerProgressSnapshot) -> Result<Option<Cursor>> {
        ensure!(
            snapshot.consumers.keys().eq(self.members.iter()),
            "SQL Server consumer membership changed"
        );
        let mut minimum: Option<Cursor> = None;
        let mut missing = 0;
        for (id, state) in &snapshot.consumers {
            if let Some(failure) = &state.failure {
                anyhow::bail!("SQL Server consumer {id} failed: {failure}");
            }
            ensure!(state.ready, "SQL Server consumer {id} lost readiness");
            let checkpoint = state
                .checkpoints
                .get(&SourceProgressKey::Source(self.descriptor.id().to_string()));
            let cursor = checkpoint
                .map(|cp| {
                    Cursor::decode(
                        cp.source_position
                            .as_deref()
                            .context("SQL Server checkpoint has no CDC position")?,
                    )
                })
                .transpose()?;
            if let Some((generation, previous)) = self.observed.get(id) {
                ensure!(
                    *generation == state.reset_generation,
                    "SQL Server consumer reset during activation; reconstruct the source"
                );
                if let Some(previous) = previous {
                    ensure!(
                        cursor
                            .as_ref()
                            .context("SQL Server consumer cursor disappeared")?
                            .compare(previous)?
                            != Ordering::Less,
                        "SQL Server consumer cursor regressed"
                    );
                }
            }
            self.observed
                .insert(id.clone(), (state.reset_generation, cursor.clone()));
            self.sequence = self.sequence.max(checkpoint.map_or(0, |cp| cp.sequence));
            self.sequence = self.sequence.max(
                state
                    .transport_sequences
                    .get(&self.stream)
                    .copied()
                    .unwrap_or(0),
            );
            if let Some(cursor) = cursor {
                if minimum
                    .as_ref()
                    .map(|min| cursor.compare(min))
                    .transpose()?
                    .is_none_or(|order| order == Ordering::Less)
                {
                    minimum = Some(cursor);
                }
            } else {
                missing += 1;
            }
        }
        if missing > 0 && missing < self.members.len() {
            let origin = self.origin.as_ref().context(
                "SQL Server mixed fresh/resumed consumers require coordinated bootstrap or a retained actual activation boundary",
            )?;
            if minimum
                .as_ref()
                .map(|cursor| origin.compare(cursor))
                .transpose()?
                .is_none_or(|order| order == Ordering::Less)
            {
                minimum = Some(origin.clone());
            }
        }
        Ok(minimum)
    }
}

#[async_trait]
impl NativeComponent for MsSqlSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(Configuration {
            stream: self.stream.to_string(),
            settings: self.config.clone(),
        })?)
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        Some(self.progress.clone())
    }
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        Some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        Some(self.policy.clone())
    }
    async fn start(&mut self) -> Result<()> {
        ensure!(
            !self.cleanup,
            "SQL Server source requires successful stop before restart"
        );
        self.cleanup = true;
        self.initialized = false;
        self.generation.fetch_add(1, AtomicOrdering::AcqRel);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        if self.generation.load(AtomicOrdering::Acquire) % 2 == 1 {
            self.generation.fetch_add(1, AtomicOrdering::AcqRel);
        }
        self.work.stop(self.config.shutdown_timeout()).await?;
        self.queue.clear();
        self.cursor = None;
        self.observed.clear();
        self.initialized = false;
        self.cleanup = false;
        Ok(())
    }
}

impl Drop for MsSqlSource {
    fn drop(&mut self) {
        if self.generation.load(AtomicOrdering::Acquire) % 2 == 1 {
            self.generation.fetch_add(1, AtomicOrdering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{
        ConsumerProgressProvider, ConsumerProgressUpdates, SourceProgressSnapshot,
    };
    use drasi_core_types::interface::SourceCheckpoint;

    struct Progress(ComponentId);
    impl ConsumerProgressProvider for Progress {
        fn graph_id(&self) -> &str {
            "graph"
        }
        fn producer_id(&self) -> &ComponentId {
            &self.0
        }
        fn snapshot(&self) -> Result<ConsumerProgressSnapshot> {
            anyhow::bail!("unit test does not read external progress")
        }
        fn subscribe(&self) -> Result<Box<dyn ConsumerProgressUpdates>> {
            anyhow::bail!("unit test does not subscribe")
        }
    }
    fn fixture() -> Result<MsSqlSource> {
        let source = ComponentId::try_new("source")?;
        MsSqlSource::new(
            source.clone(),
            StreamId::try_new("source/out")?,
            serde_json::from_value(
                serde_json::json!({"connection":{"host":"no-network-at-construction","database":"d","user":"u"}}),
            )?,
            ConsumerProgressView::new(Arc::new(Progress(source))),
            &[ComponentId::try_new("a")?, ComponentId::try_new("b")?],
        )
    }
    fn snapshot(first: Option<u8>, second: Option<u8>) -> Result<ConsumerProgressSnapshot> {
        let mut consumers = BTreeMap::new();
        for (name, lsn) in [("a", first), ("b", second)] {
            let mut state = SourceProgressSnapshot {
                ready: true,
                ..Default::default()
            };
            if let Some(lsn) = lsn {
                state.checkpoints.insert(
                    SourceProgressKey::Source("source".into()),
                    SourceCheckpoint::new(
                        if name == "a" { 100 } else { 5 },
                        Some(Cursor::boundary([7; 32], [lsn; 10]).encode()?.into()),
                    ),
                );
            }
            state
                .transport_sequences
                .insert(StreamId::try_new("source/out")?, 1000);
            consumers.insert(ComponentId::try_new(name)?, Arc::new(state));
        }
        Ok(ConsumerProgressSnapshot { consumers })
    }
    #[test]
    fn minimum_cursor_uses_actual_slowest_consumer_not_logical_or_transport_counters() -> Result<()>
    {
        let mut source = fixture()?;
        assert_eq!(
            source.observe(&snapshot(Some(4), Some(2))?)?.unwrap().lsn,
            [2; 10]
        );
        assert_eq!(source.sequence, 1000);
        assert_eq!(
            source.observe(&snapshot(Some(5), Some(3))?)?.unwrap().lsn,
            [3; 10]
        );
        assert!(source.observe(&snapshot(Some(5), Some(2))?).is_err());
        Ok(())
    }
    #[test]
    fn missing_failed_reset_and_incomparable_consumers_never_get_skipped() -> Result<()> {
        assert!(fixture()?.observe(&snapshot(Some(1), None)?).is_err());
        let mut retained = fixture()?;
        retained.origin = Some(Cursor::boundary([7; 32], [1; 10]));
        assert_eq!(
            retained.observe(&snapshot(Some(3), None)?)?.unwrap().lsn,
            [1; 10]
        );
        let mut missing = snapshot(Some(1), Some(1))?;
        missing.consumers.remove(&ComponentId::try_new("b")?);
        assert!(fixture()?.observe(&missing).is_err());
        for change in [
            "failure",
            "reset",
            "binding",
            "missing-position",
            "not-ready",
        ] {
            let mut source = fixture()?;
            source.observe(&snapshot(Some(1), Some(1))?)?;
            let mut next = snapshot(Some(2), Some(2))?;
            let member =
                Arc::make_mut(next.consumers.get_mut(&ComponentId::try_new("b")?).unwrap());
            match change {
                "failure" => member.failure = Some("failed".into()),
                "reset" => member.reset_generation += 1,
                "binding" => {
                    member
                        .checkpoints
                        .values_mut()
                        .next()
                        .unwrap()
                        .source_position = Some(Cursor::boundary([8; 32], [2; 10]).encode()?.into())
                }
                "missing-position" => {
                    member
                        .checkpoints
                        .values_mut()
                        .next()
                        .unwrap()
                        .source_position = None
                }
                _ => member.ready = false,
            }
            assert!(source.observe(&next).is_err(), "{change}");
        }
        Ok(())
    }
    #[tokio::test]
    async fn schema_reader_is_stable_at_creation_and_fences_retired_source() -> Result<()> {
        let mut source = fixture()?;
        let first = source
            .source_schema_reader()
            .context("schema reader missing at construction")?;
        let second = source.source_schema_reader().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first
            .describe_schema()
            .await
            .unwrap_err()
            .to_string()
            .contains("retired"));
        source.start().await?;
        assert!(source.start().await.is_err());
        source.stop().await?;
        assert!(first
            .describe_schema()
            .await
            .unwrap_err()
            .to_string()
            .contains("retired"));
        source.start().await?;
        source.stop().await?;
        Ok(())
    }
    #[tokio::test]
    async fn retained_schema_reader_is_fenced_when_source_is_dropped() -> Result<()> {
        let mut source = fixture()?;
        let reader = source.source_schema_reader().context("schema reader")?;
        source.start().await?;
        drop(source);
        assert!(reader
            .describe_schema()
            .await
            .expect_err("retired reader")
            .to_string()
            .contains("retired"));
        Ok(())
    }
}
