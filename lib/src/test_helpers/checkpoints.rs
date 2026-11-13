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

use drasi_core::interface::CheckpointStore;
use std::sync::Arc;

pub(crate) struct QueryCheckpointView<'a>(&'a Arc<dyn crate::queries::Query>);
pub(crate) fn query(query: &Arc<dyn crate::queries::Query>) -> QueryCheckpointView<'_> {
    QueryCheckpointView(query)
}
impl QueryCheckpointView<'_> {
    pub(crate) async fn get_checkpoint_store(&self) -> Option<Arc<dyn CheckpointStore>> {
        store(self.0).await
    }
}

pub(crate) async fn store(
    query: &Arc<dyn crate::queries::Query>,
) -> Option<Arc<dyn CheckpointStore>> {
    raw_store(query)
        .await
        .map(|inner| Arc::new(NativeCheckpoints(inner)) as Arc<dyn CheckpointStore>)
}

pub(crate) async fn raw_store(
    query: &Arc<dyn crate::queries::Query>,
) -> Option<Arc<dyn CheckpointStore>> {
    query
        .as_any()
        .downcast_ref::<crate::computation::runtime::NativeQuery>()
        .expect("computation query")
        .checkpoint_store()
        .expect("native checkpoint resource")
}

pub(crate) fn storage_scope(query: &crate::ComponentId) -> crate::StorageScope {
    crate::computation::v1::LegacyIndexProviderAdapter::storage_key(
        Some("test-instance"),
        crate::computation::v1::ComponentBatch::GRAPH_ID,
        query,
    )
}

/// A progress-only view over the canonical source and stream checkpoint keys.
struct NativeCheckpoints(Arc<dyn CheckpointStore>);
#[async_trait::async_trait]
impl CheckpointStore for NativeCheckpoints {
    fn is_persistent(&self) -> bool {
        self.0.is_persistent()
    }
    async fn stage_checkpoint(
        &self,
        source: &drasi_core_types::CheckpointKey,
        sequence: u64,
        position: Option<&bytes::Bytes>,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.0.stage_checkpoint(source, sequence, position).await
    }
    async fn read_checkpoint(
        &self,
        source: &drasi_core_types::CheckpointKey,
    ) -> Result<Option<drasi_core::interface::SourceCheckpoint>, drasi_core::interface::IndexError>
    {
        self.0.read_checkpoint(source).await
    }
    async fn read_all_checkpoints(
        &self,
    ) -> Result<
        std::collections::HashMap<
            drasi_core_types::CheckpointKey,
            drasi_core::interface::SourceCheckpoint,
        >,
        drasi_core::interface::IndexError,
    > {
        Ok(self
            .0
            .read_all_checkpoints()
            .await?
            .into_iter()
            .filter(|(key, _)| key.is_progress())
            .collect())
    }
    async fn clear_checkpoints(&self) -> Result<(), drasi_core::interface::IndexError> {
        self.0.clear_checkpoints().await
    }
    async fn write_config_hash(&self, hash: u64) -> Result<(), drasi_core::interface::IndexError> {
        self.0.write_config_hash(hash).await
    }
    async fn read_config_hash(&self) -> Result<Option<u64>, drasi_core::interface::IndexError> {
        self.0.read_config_hash().await
    }
    async fn stage_result_sequence(
        &self,
        query: &drasi_core_types::OutputPartition,
        sequence: u64,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.0.stage_result_sequence(query, sequence).await
    }
    async fn write_result_sequence(
        &self,
        query: &drasi_core_types::OutputPartition,
        sequence: u64,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.0.write_result_sequence(query, sequence).await
    }
    async fn read_result_sequence(
        &self,
        query: &drasi_core_types::OutputPartition,
    ) -> Result<Option<u64>, drasi_core::interface::IndexError> {
        self.0.read_result_sequence(query).await
    }
    async fn write_output_generation(
        &self,
        query: &drasi_core_types::OutputPartition,
        generation: u64,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.0.write_output_generation(query, generation).await
    }
    async fn read_output_generation(
        &self,
        query: &drasi_core_types::OutputPartition,
    ) -> Result<Option<u64>, drasi_core::interface::IndexError> {
        self.0.read_output_generation(query).await
    }
}
