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
    #[cfg(feature = "computation")]
    if let Some(query) = query
        .as_any()
        .downcast_ref::<crate::computation::compatibility::QueryInstance>()
    {
        return query
            .checkpoint_store()
            .expect("native checkpoint resource")
            .map(|inner| Arc::new(NativeCheckpoints(inner)) as Arc<dyn CheckpointStore>);
    }
    query
        .as_any()
        .downcast_ref::<crate::queries::DrasiQuery>()
        .expect("legacy query implementation")
        .get_checkpoint_store()
        .await
}

pub(crate) fn storage_id(query: &str) -> String {
    #[cfg(feature = "computation")]
    if super::execution_mode() == crate::ExecutionMode::ComputationGraph
        && !query.starts_with("computation-v2-")
    {
        let graph = format!(
            "lib-query/{}",
            query
                .bytes()
                .map(|value| format!("{value:02x}"))
                .collect::<String>()
        );
        return crate::computation::v1::LegacyIndexProviderAdapter::storage_key(
            Some("test-instance"),
            &graph,
            query,
        );
    }
    query.to_owned()
}

/// A semantic checkpoint view: translate storage keys, never checkpoint values.
#[cfg(feature = "computation")]
struct NativeCheckpoints(Arc<dyn CheckpointStore>);
#[cfg(feature = "computation")]
#[async_trait::async_trait]
impl CheckpointStore for NativeCheckpoints {
    fn is_persistent(&self) -> bool {
        self.0.is_persistent()
    }
    async fn stage_checkpoint(
        &self,
        source: &str,
        sequence: u64,
        position: Option<&bytes::Bytes>,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.0
            .stage_checkpoint(
                &crate::computation::v1::progress_key("", Some(source)),
                sequence,
                position,
            )
            .await
    }
    async fn read_checkpoint(
        &self,
        source: &str,
    ) -> Result<Option<drasi_core::interface::SourceCheckpoint>, drasi_core::interface::IndexError>
    {
        self.0
            .read_checkpoint(&crate::computation::v1::progress_key("", Some(source)))
            .await
    }
    async fn read_all_checkpoints(
        &self,
    ) -> Result<
        std::collections::HashMap<String, drasi_core::interface::SourceCheckpoint>,
        drasi_core::interface::IndexError,
    > {
        let mut result = std::collections::HashMap::new();
        for (key, checkpoint) in self.0.read_all_checkpoints().await? {
            if !key.starts_with("computation:input:") {
                continue;
            }
            match crate::computation::v1::progress_identity(&key).map_err(|error| {
                drasi_core::interface::IndexError::Other(error.into_boxed_dyn_error())
            })? {
                crate::computation::v1::SourceProgressKey::Source(source) => {
                    result.insert(source, checkpoint);
                }
                crate::computation::v1::SourceProgressKey::Stream(stream) => {
                    result.insert(stream.as_str().to_owned(), checkpoint);
                }
            }
        }
        Ok(result)
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
    async fn write_result_sequence(
        &self,
        query: &str,
        sequence: u64,
    ) -> Result<(), drasi_core::interface::IndexError> {
        self.0.write_result_sequence(query, sequence).await
    }
    async fn read_result_sequence(
        &self,
        query: &str,
    ) -> Result<Option<u64>, drasi_core::interface::IndexError> {
        self.0.read_result_sequence(query).await
    }
}
