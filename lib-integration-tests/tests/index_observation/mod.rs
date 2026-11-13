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

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::interface::{
    CheckpointKey, CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError,
    SourceCheckpoint, StorageScope,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::PortId;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Inspect the actual native checkpoint format, not the retired linked-query keys.
pub async fn native_source_checkpoints(
    store: &dyn CheckpointStore,
) -> Result<BTreeMap<drasi_core::ComponentId, SourceCheckpoint>> {
    let mut sources = BTreeMap::new();
    for (key, checkpoint) in store.read_all_checkpoints().await? {
        let CheckpointKey::Associated {
            checkpoint: identity,
            name,
        } = key
        else {
            continue;
        };
        let Some(encoded) = name.strip_prefix("\0computation:processing-input:v1:") else {
            continue;
        };
        let _: PortId =
            serde_json::from_str(encoded).context("decode native input checkpoint port")?;
        match *identity {
            CheckpointKey::Source(source) => {
                anyhow::ensure!(
                    sources.insert(source, checkpoint).is_none(),
                    "fixture has duplicate source checkpoint owners"
                );
            }
            CheckpointKey::Stream(_) => {}
            _ => anyhow::bail!("native input checkpoint has an invalid identity domain"),
        }
    }
    Ok(sources)
}

#[derive(Deserialize, Serialize)]
struct IndexLocation {
    storage_scope: StorageScope,
    query_id: drasi_core::ComponentId,
}

pub struct ObservedIndexProvider {
    inner: Arc<dyn IndexBackendPlugin>,
    location: PathBuf,
    query_id: drasi_core::ComponentId,
}

impl ObservedIndexProvider {
    pub fn new(
        inner: Arc<dyn IndexBackendPlugin>,
        root: &Path,
        query_id: drasi_core::ComponentId,
    ) -> Self {
        Self {
            inner,
            location: root.join("observed-query-index.json"),
            query_id,
        }
    }
}

#[async_trait]
impl IndexBackendPlugin for ObservedIndexProvider {
    async fn create_indexes(
        &self,
        query_id: &drasi_core::ComponentId,
    ) -> Result<CreatedIndexes, IndexError> {
        self.create_scoped_indexes(&StorageScope::default(), query_id)
            .await
    }

    async fn create_scoped_indexes(
        &self,
        storage_scope: &drasi_core::interface::StorageScope,
        query_id: &drasi_core::ComponentId,
    ) -> Result<CreatedIndexes, IndexError> {
        let indexes = self
            .inner
            .create_scoped_indexes(storage_scope, query_id)
            .await?;
        if query_id == &self.query_id {
            let location = IndexLocation {
                storage_scope: storage_scope.clone(),
                query_id: query_id.clone(),
            };
            let bytes = serde_json::to_vec(&location).map_err(IndexError::other)?;
            let mut file = std::fs::File::create(&self.location).map_err(IndexError::other)?;
            file.write_all(&bytes).map_err(IndexError::other)?;
            file.sync_all().map_err(IndexError::other)?;
        }
        Ok(indexes)
    }

    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }

    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.inner.durability()
    }

    fn supports_atomic_query_output(&self) -> bool {
        self.inner.supports_atomic_query_output()
    }
}

pub async fn open_query_indexes(
    root: &Path,
    query_id: &drasi_core::ComponentId,
) -> Result<CreatedIndexes> {
    let location: IndexLocation = serde_json::from_slice(
        &std::fs::read(root.join("observed-query-index.json"))
            .context("read the runtime's observed index partition")?,
    )
    .context("decode the observed index partition")?;
    anyhow::ensure!(
        &location.query_id == query_id
            && location.storage_scope != drasi_core::interface::StorageScope::default(),
        "query must use its own observed computation storage partition"
    );
    RocksDbIndexProvider::new(root, false, false)
        .create_scoped_indexes(&location.storage_scope, query_id)
        .await
        .context("open the runtime's observed index partition")
}
