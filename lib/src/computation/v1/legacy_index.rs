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

use super::{QueryIndexProviderResource, ResourceCleanup, ResourceHandle, ResourceRole};
use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationIoScope, ComputationResource,
        ScopedIndex,
    },
    interface::{
        CheckpointStore, IndexBackendPlugin, IndexError, IndexSet, LiveResultsWriter, OutboxWriter,
    },
};
use std::sync::Arc;

/// Reuses an existing index plugin in an isolated computation namespace. Its
/// legacy writers are never inferred to participate in one atomic transaction.
/// Use explicit non-atomic publication unless a native provider proves more.
pub struct LegacyIndexProviderAdapter {
    provider: Arc<dyn IndexBackendPlugin>,
    construction: Arc<ComputationIoScope>,
    namespace: Option<Arc<str>>,
}

impl LegacyIndexProviderAdapter {
    pub(crate) fn storage_key(namespace: Option<&str>, graph: &str, query: &str) -> String {
        let encode = |value: &str| {
            value
                .bytes()
                .map(|value| format!("{value:02x}"))
                .collect::<String>()
        };
        match namespace {
            Some(scope) => format!(
                "computation-v2-{}-{}-{}",
                encode(scope),
                encode(graph),
                encode(query)
            ),
            None => format!("computation-v1-{}-{}", encode(graph), encode(query)),
        }
    }
    pub fn new(provider: Arc<dyn IndexBackendPlugin>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            construction: Arc::new(ComputationIoScope::default()),
            namespace: None,
        })
    }
    /// Isolate instances that deliberately share one underlying backend plugin.
    pub fn scoped(
        provider: Arc<dyn IndexBackendPlugin>,
        namespace: impl Into<Arc<str>>,
    ) -> super::Result<Arc<Self>> {
        let namespace = namespace.into();
        super::data::validate_identifier("index namespace", &namespace)?;
        Ok(Arc::new(Self {
            provider,
            construction: Arc::new(ComputationIoScope::default()),
            namespace: Some(namespace),
        }))
    }
    /// The adapter owns constructor work; register this handle as graph-owned.
    /// The underlying instance-provided plugin/factory is only borrowed.
    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(
            ResourceRole::IndexBackend,
            Arc::new(QueryIndexProviderResource(self.clone())),
        )
        .with_cleanup(self.clone())
    }
}

#[async_trait]
impl ResourceCleanup for LegacyIndexProviderAdapter {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.construction.shutdown().await?;
        Ok(())
    }
}

#[async_trait]
impl ComputationIndexProvider for LegacyIndexProviderAdapter {
    async fn create_indexes(
        &self,
        graph: &str,
        query: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        super::data::validate_identifier("graph", graph).map_err(IndexError::other)?;
        super::data::validate_identifier("query", query).map_err(IndexError::other)?;
        let key = Self::storage_key(self.namespace.as_deref(), graph, query);
        let provider = self.provider.clone();
        let original = self
            .construction
            .run_async(async move { provider.create_indexes(&key).await })
            .await?;
        let work = Arc::new(ComputationIoScope::default());
        let checkpoint = original.checkpoint_store.map(|value| {
            ComputationResource::independent(
                Arc::new(ScopedIndex::from_arc(value, work.clone())) as Arc<dyn CheckpointStore>
            )
        });
        let outbox = original.outbox_writer.map(|value| {
            ComputationResource::independent(
                Arc::new(ScopedIndex::from_arc(value, work.clone())) as Arc<dyn OutboxWriter>
            )
        });
        let live = original.live_results_writer.map(|value| {
            ComputationResource::independent(
                Arc::new(ScopedIndex::from_arc(value, work.clone())) as Arc<dyn LiveResultsWriter>
            )
        });
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: Arc::new(ScopedIndex::from_arc(
                    original.set.element_index,
                    work.clone(),
                )),
                archive_index: Arc::new(ScopedIndex::from_arc(
                    original.set.archive_index,
                    work.clone(),
                )),
                result_index: Arc::new(ScopedIndex::from_arc(
                    original.set.result_index,
                    work.clone(),
                )),
                future_queue: Arc::new(ScopedIndex::from_arc(
                    original.set.future_queue,
                    work.clone(),
                )),
                session_control: Arc::new(ScopedIndex::from_arc(
                    original.set.session_control,
                    work.clone(),
                )),
            },
            None,
            checkpoint,
            outbox,
            live,
        )
        .map_err(IndexError::other)?;
        Ok(indexes.with_cleanup(work))
    }
    fn is_volatile(&self) -> bool {
        self.provider.is_volatile()
    }
}
