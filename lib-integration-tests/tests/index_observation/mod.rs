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
use drasi_core::interface::{CreatedIndexes, IndexBackendPlugin, IndexError};
use drasi_index_rocksdb::RocksDbIndexProvider;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Deserialize, Serialize)]
struct IndexLocation {
    storage_scope: String,
    query_id: String,
}

pub struct ObservedIndexProvider {
    inner: Arc<dyn IndexBackendPlugin>,
    location: PathBuf,
    query_id: String,
}

impl ObservedIndexProvider {
    pub fn new(inner: Arc<dyn IndexBackendPlugin>, root: &Path, query_id: &str) -> Self {
        Self {
            inner,
            location: root.join("observed-query-index.json"),
            query_id: query_id.to_owned(),
        }
    }
}

#[async_trait]
impl IndexBackendPlugin for ObservedIndexProvider {
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        self.create_scoped_indexes(query_id, query_id).await
    }

    async fn create_scoped_indexes(
        &self,
        storage_scope: &str,
        query_id: &str,
    ) -> Result<CreatedIndexes, IndexError> {
        let indexes = self
            .inner
            .create_scoped_indexes(storage_scope, query_id)
            .await?;
        if query_id == self.query_id {
            let location = IndexLocation {
                storage_scope: storage_scope.to_owned(),
                query_id: query_id.to_owned(),
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

    fn supports_atomic_query_output(&self) -> bool {
        self.inner.supports_atomic_query_output()
    }
}

pub async fn open_query_indexes(root: &Path, query_id: &str) -> Result<CreatedIndexes> {
    let location: IndexLocation = serde_json::from_slice(
        &std::fs::read(root.join("observed-query-index.json"))
            .context("read the runtime's observed index partition")?,
    )
    .context("decode the observed index partition")?;
    anyhow::ensure!(
        location.query_id == query_id && location.storage_scope != query_id,
        "query must use its own observed computation storage partition"
    );
    RocksDbIndexProvider::new(root, false, false)
        .create_scoped_indexes(&location.storage_scope, query_id)
        .await
        .context("open the runtime's observed index partition")
}
