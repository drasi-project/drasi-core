// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource,
        ComputationResourceCleanup, ComputationTransaction,
    },
    interface::{
        CheckpointStore, IndexError, IndexSet, OutboxPageLimits, OutboxWriter, StorageDurability,
    },
};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Weak,
};

pub(super) struct SplitOutputProvider {
    indexes: Arc<RocksDbComputationProvider>,
    output: Arc<RocksDbComputationProvider>,
    failure: Arc<AtomicBool>,
    exit: Arc<AtomicU8>,
    checkpoints: Mutex<Option<Weak<dyn CheckpointStore>>>,
    output_owner: Mutex<Option<Weak<ComputationTransaction>>>,
}

impl SplitOutputProvider {
    pub(super) fn new(path: &Path) -> Result<Self> {
        Ok(Self {
            indexes: provider(&path.join("indexes"))?,
            output: provider(&path.join("output"))?,
            failure: Arc::new(AtomicBool::new(false)),
            exit: Arc::new(AtomicU8::new(0)),
            checkpoints: Mutex::new(None),
            output_owner: Mutex::new(None),
        })
    }

    pub(super) fn fail_after_next_output_commit(&self) {
        self.failure.store(true, Ordering::Release);
    }

    pub(super) fn storage_providers(
        &self,
    ) -> (
        Arc<RocksDbComputationProvider>,
        Arc<RocksDbComputationProvider>,
    ) {
        (self.indexes.clone(), self.output.clone())
    }

    pub(super) fn exit_after_output_commit(&self) {
        self.exit.store(1, Ordering::Release);
    }

    pub(super) fn exit_after_output_clear(&self) {
        self.exit.store(2, Ordering::Release);
    }

    pub(super) async fn stored_publication(
        &self,
    ) -> Result<drasi_core::interface::SourceCheckpoint> {
        let checkpoints = self
            .checkpoints
            .lock()
            .expect("checkpoint observation")
            .as_ref()
            .and_then(Weak::upgrade)
            .context("live checkpoint owner")?;
        checkpoints
            .read_checkpoint(&drasi_core::interface::CheckpointKey::Metadata(
                drasi_query_common::PENDING_QUERY_OUTPUT.to_owned(),
            ))
            .await?
            .context("publication marker")
    }

    pub(super) async fn independently_committed_output(&self) -> Result<Option<u64>> {
        let owner = self
            .output_owner
            .lock()
            .expect("output observation")
            .as_ref()
            .and_then(Weak::upgrade)
            .context("live independent output owner")?;
        Ok(owner
            .resources()
            .outbox_writer()
            .context("independent output")?
            .read_latest_sequence(&output_partition()?)
            .await?)
    }
}

fn index_error(error: impl Into<anyhow::Error>) -> IndexError {
    IndexError::Other(error.into().into_boxed_dyn_error())
}

#[async_trait]
impl ComputationIndexProvider for SplitOutputProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &drasi_core_types::ComponentId,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let primary = self.indexes.create_indexes(graph, component).await?;
        let output = Arc::new(
            ComputationTransaction::try_new(self.output.create_indexes(graph, component).await?)
                .map_err(index_error)?,
        );
        let writer = Arc::new(IndependentOutbox {
            writer: output
                .resources()
                .outbox_writer()
                .expect("RocksDB outbox")
                .clone(),
            owner: output.clone(),
            failure: self.failure.clone(),
            exit: self.exit.clone(),
            component: drasi_core_types::OutputPartition::Component(component.clone()),
        });
        let checkpoint = primary
            .checkpoint_store()
            .expect("RocksDB checkpoints")
            .clone();
        *self
            .checkpoints
            .lock()
            .map_err(|_| IndexError::CorruptedData)? = Some(Arc::downgrade(&checkpoint));
        *self
            .output_owner
            .lock()
            .map_err(|_| IndexError::CorruptedData)? = Some(Arc::downgrade(&output));
        let indexes = primary.indexes();
        let resources = ComputationIndexes::try_new(
            IndexSet {
                element_index: indexes.element_index.clone(),
                archive_index: indexes.archive_index.clone(),
                result_index: indexes.result_index.clone(),
                future_queue: indexes.future_queue.clone(),
                session_control: indexes.session_control.clone(),
            },
            None,
            Some(ComputationResource::independent(checkpoint)),
            Some(ComputationResource::independent(writer)),
            Some(ComputationResource::independent(
                primary
                    .live_results_writer()
                    .expect("RocksDB live rows")
                    .clone(),
            )),
        )
        .map_err(index_error)?
        .with_durability(primary.durability())
        .with_cleanup(Arc::new(SplitCleanup {
            indexes: primary.cleanup().cloned(),
            output,
        }));
        assert!(resources.atomic_result_transaction().is_err());
        Ok(resources)
    }

    fn is_volatile(&self) -> bool {
        false
    }
    fn durability(&self) -> StorageDurability {
        self.indexes.durability()
    }
}

struct SplitCleanup {
    indexes: Option<Arc<dyn ComputationResourceCleanup>>,
    output: Arc<ComputationTransaction>,
}

#[async_trait]
impl ComputationResourceCleanup for SplitCleanup {
    fn cancel(&self) {
        if let Some(cleanup) = &self.indexes {
            cleanup.cancel();
        }
        if let Some(cleanup) = self.output.resources().cleanup() {
            cleanup.cancel();
        }
    }
    async fn shutdown(&self) -> std::result::Result<(), IndexError> {
        if let Some(cleanup) = &self.indexes {
            cleanup.shutdown().await?;
        }
        self.output.shutdown().await.map_err(index_error)
    }
    async fn quiesce(&self) -> std::result::Result<(), IndexError> {
        if let Some(cleanup) = &self.indexes {
            cleanup.quiesce().await?;
        }
        self.output.quiesce().await.map_err(index_error)
    }
}

struct IndependentOutbox {
    owner: Arc<ComputationTransaction>,
    writer: Arc<dyn OutboxWriter>,
    failure: Arc<AtomicBool>,
    exit: Arc<AtomicU8>,
    component: drasi_core_types::OutputPartition,
}

impl IndependentOutbox {
    fn after_commit(
        &self,
        component: &drasi_core_types::OutputPartition,
    ) -> std::result::Result<(), IndexError> {
        if component == &self.component
            && self
                .exit
                .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            std::process::exit(86);
        }
        if component == &self.component && self.failure.swap(false, Ordering::AcqRel) {
            return Err(index_error(anyhow::anyhow!(
                "injected failure after independent output commit"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl OutboxWriter for IndependentOutbox {
    async fn append(
        &self,
        query: &drasi_core_types::OutputPartition,
        sequence: u64,
        data: &[u8],
    ) -> std::result::Result<(), IndexError> {
        self.owner
            .run(async {
                self.writer
                    .append(query, sequence, data)
                    .await
                    .map_err(Into::into)
            })
            .await
            .map_err(index_error)?;
        self.after_commit(query)
    }
    async fn append_and_trim(
        &self,
        query: &drasi_core_types::OutputPartition,
        sequence: u64,
        data: &[u8],
        retain_from: u64,
    ) -> std::result::Result<usize, IndexError> {
        let removed = self
            .owner
            .run(async {
                self.writer
                    .append_and_trim(query, sequence, data, retain_from)
                    .await
                    .map_err(Into::into)
            })
            .await
            .map_err(index_error)?;
        self.after_commit(query)?;
        Ok(removed)
    }
    async fn read_from(
        &self,
        query: &drasi_core_types::OutputPartition,
        after: u64,
    ) -> std::result::Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.writer.read_from(query, after).await
    }
    async fn read_page(
        &self,
        query: &drasi_core_types::OutputPartition,
        after: u64,
        limits: OutboxPageLimits,
    ) -> std::result::Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.writer.read_page(query, after, limits).await
    }
    async fn read_latest_sequence(
        &self,
        query: &drasi_core_types::OutputPartition,
    ) -> std::result::Result<Option<u64>, IndexError> {
        self.writer.read_latest_sequence(query).await
    }
    async fn clear(
        &self,
        query: &drasi_core_types::OutputPartition,
    ) -> std::result::Result<(), IndexError> {
        self.owner
            .run(async { self.writer.clear(query).await.map_err(Into::into) })
            .await
            .map_err(index_error)?;
        if query == &self.component
            && self
                .exit
                .compare_exchange(2, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            std::process::exit(86);
        }
        Ok(())
    }
    async fn trim_before(
        &self,
        query: &drasi_core_types::OutputPartition,
        retain: u64,
    ) -> std::result::Result<usize, IndexError> {
        self.owner
            .run(async {
                self.writer
                    .trim_before(query, retain)
                    .await
                    .map_err(Into::into)
            })
            .await
            .map_err(index_error)
    }
    async fn trim_to_capacity(
        &self,
        query: &drasi_core_types::OutputPartition,
        capacity: usize,
    ) -> std::result::Result<usize, IndexError> {
        self.owner
            .run(async {
                self.writer
                    .trim_to_capacity(query, capacity)
                    .await
                    .map_err(Into::into)
            })
            .await
            .map_err(index_error)
    }
}
