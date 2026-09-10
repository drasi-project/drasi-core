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

use super::ScopedIndex;
use crate::interface::{
    CheckpointStore, IndexError, LiveResultsWriter, OutboxWriter, RowMutation, SessionControl,
    SourceCheckpoint,
};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

#[async_trait]
impl<T: SessionControl + ?Sized + 'static> SessionControl for ScopedIndex<T> {
    async fn begin(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.begin().await })
            .await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.commit().await })
            .await
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

#[async_trait]
impl<T: CheckpointStore + ?Sized + 'static> CheckpointStore for ScopedIndex<T> {
    fn is_persistent(&self) -> bool {
        self.inner.is_persistent()
    }
    async fn stage_checkpoint(
        &self,
        source: &str,
        sequence: u64,
        position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let source = source.to_owned();
        let position = position.cloned();
        self.work
            .run_async(async move {
                inner
                    .stage_checkpoint(&source, sequence, position.as_ref())
                    .await
            })
            .await
    }
    async fn read_checkpoint(&self, source: &str) -> Result<Option<SourceCheckpoint>, IndexError> {
        let inner = self.inner.clone();
        let source = source.to_owned();
        self.work
            .run_async(async move { inner.read_checkpoint(&source).await })
            .await
    }
    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.read_all_checkpoints().await })
            .await
    }
    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.clear_checkpoints().await })
            .await
    }
    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.write_config_hash(hash).await })
            .await
    }
    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.read_config_hash().await })
            .await
    }
    async fn write_result_sequence(&self, query: &str, sequence: u64) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.write_result_sequence(&query, sequence).await })
            .await
    }
    async fn read_result_sequence(&self, query: &str) -> Result<Option<u64>, IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.read_result_sequence(&query).await })
            .await
    }
}

#[async_trait]
impl<T: OutboxWriter + ?Sized + 'static> OutboxWriter for ScopedIndex<T> {
    async fn append(&self, query: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        let data = data.to_vec();
        self.work
            .run_async(async move { inner.append(&query, sequence, &data).await })
            .await
    }
    async fn read_from(&self, query: &str, after: u64) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.read_from(&query, after).await })
            .await
    }
    async fn read_latest_sequence(&self, query: &str) -> Result<Option<u64>, IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.read_latest_sequence(&query).await })
            .await
    }
    async fn clear(&self, query: &str) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.clear(&query).await })
            .await
    }
    async fn trim_to_capacity(&self, query: &str, capacity: usize) -> Result<usize, IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.trim_to_capacity(&query, capacity).await })
            .await
    }
}

#[async_trait]
impl<T: LiveResultsWriter + ?Sized + 'static> LiveResultsWriter for ScopedIndex<T> {
    async fn apply_mutations(
        &self,
        query: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        let mutations: Vec<_> = mutations
            .iter()
            .map(|row| (row.row_signature, row.data.map(Vec::from)))
            .collect();
        self.work
            .run_async(async move {
                let rows: Vec<_> = mutations
                    .iter()
                    .map(|(row_signature, data)| RowMutation {
                        row_signature: *row_signature,
                        data: data.as_deref(),
                    })
                    .collect();
                inner.apply_mutations(&query, &rows).await
            })
            .await
    }
    async fn read_snapshot(&self, query: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.read_snapshot(&query).await })
            .await
    }
    async fn clear(&self, query: &str) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.clear(&query).await })
            .await
    }
    async fn row_count(&self, query: &str) -> Result<usize, IndexError> {
        let inner = self.inner.clone();
        let query = query.to_owned();
        self.work
            .run_async(async move { inner.row_count(&query).await })
            .await
    }
}
