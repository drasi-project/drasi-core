// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{result::Result, sync::Arc};

use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes, ComputationTransactionGroup},
    interface::{IndexError, StorageDurability},
};

use super::*;

/// Graph-owned storage for one producer and its independently owned QoS journals.
/// Supply a dedicated bundle; adopting an existing standalone store is not a migration.
pub struct SharedStorageGroup {
    graph: String,
    processor: ComponentId,
    owner: ComputationTransactionGroup,
}

impl SharedStorageGroup {
    pub fn new(
        graph: impl Into<String>,
        processor: ComponentId,
        indexes: ComputationIndexes,
    ) -> anyhow::Result<Arc<Self>> {
        let graph = graph.into();
        data::validate_identifier("shared storage graph", &graph)?;
        anyhow::ensure!(
            indexes
                .checkpoint_store()
                .is_some_and(|store| store.is_persistent()),
            "shared storage requires persistent processor checkpoints"
        );
        anyhow::ensure!(
            indexes.cleanup().is_some(),
            "shared storage requires an asynchronous provider cleanup owner"
        );
        Ok(Arc::new(Self {
            graph,
            processor,
            owner: ComputationTransactionGroup::try_new(indexes)?,
        }))
    }

    /// Bind this as the producer's IndexBackend resource with graph ownership.
    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        let provider: Arc<dyn ComputationIndexProvider> = self.clone();
        ResourceHandle::new(
            ResourceRole::IndexBackend,
            Arc::new(QueryIndexProviderResource(provider.clone())),
        )
        .with_shared_identity(provider)
        .with_cleanup(self.clone())
    }

    pub async fn channel(
        &self,
        definition: QosChannelDefinition,
        journal: &str,
        codec: EnvelopeCodec,
        replay: ReplayOptions,
    ) -> Result<Arc<QosChannel>, PipeError> {
        QosChannel::shared(definition, &self.owner, journal, codec, replay).await
    }

    pub async fn channel_with_page_limits(
        &self,
        definition: QosChannelDefinition,
        journal: &str,
        codec: EnvelopeCodec,
        replay: ReplayOptions,
        page_limits: drasi_core::interface::OutboxPageLimits,
    ) -> Result<Arc<QosChannel>, PipeError> {
        QosChannel::shared_with_page_limits(
            definition,
            &self.owner,
            journal,
            codec,
            replay,
            page_limits,
        )
        .await
    }
}

#[async_trait]
impl ComputationIndexProvider for SharedStorageGroup {
    async fn create_indexes(
        &self,
        graph_id: &str,
        query_id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        if graph_id != self.graph || query_id != self.processor.as_str() {
            return Err(IndexError::other(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "shared storage belongs to a different graph or processor",
            )));
        }
        self.owner.processor_indexes().map_err(IndexError::other)
    }

    fn is_volatile(&self) -> bool {
        false
    }

    fn transaction_group(&self) -> Option<&ComputationTransactionGroup> {
        Some(&self.owner)
    }

    fn durability(&self) -> StorageDurability {
        self.owner.durability()
    }
}

#[async_trait]
impl ResourceCleanup for SharedStorageGroup {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.owner.shutdown().await?;
        Ok(())
    }
}
