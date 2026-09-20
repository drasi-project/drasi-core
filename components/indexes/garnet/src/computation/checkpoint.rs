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

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::{CheckpointStore, IndexError, SourceCheckpoint};
use redis::aio::MultiplexedConnection;

use crate::{GarnetCheckpointStore, GarnetSessionState};

pub(super) struct ComputationCheckpointStore {
    inner: GarnetCheckpointStore,
}

impl ComputationCheckpointStore {
    pub(super) fn new(
        partition: &str,
        connection: MultiplexedConnection,
        session: Arc<GarnetSessionState>,
    ) -> Self {
        Self {
            inner: GarnetCheckpointStore::new(partition, connection, session),
        }
    }
}

#[async_trait]
impl CheckpointStore for ComputationCheckpointStore {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        self.inner
            .stage_checkpoint(source_id, sequence, source_position)
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(source_id).await
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        self.inner.read_all_checkpoints().await
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        self.inner.clear_checkpoints().await
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        self.inner.write_config_hash(hash).await
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        self.inner.read_config_hash().await
    }

    async fn stage_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        self.inner.stage_result_sequence(query_id, sequence).await
    }

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        self.inner.write_result_sequence(query_id, sequence).await
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(query_id).await
    }

    async fn write_output_generation(
        &self,
        query_id: &str,
        generation: u64,
    ) -> Result<(), IndexError> {
        self.inner
            .write_output_generation(query_id, generation)
            .await
    }

    async fn read_output_generation(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_output_generation(query_id).await
    }
}
