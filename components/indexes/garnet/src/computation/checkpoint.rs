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
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::{session_state::BufferReadResult, GarnetCheckpointStore, GarnetSessionState};

pub(super) struct ComputationCheckpointStore {
    inner: GarnetCheckpointStore,
    result_sequence_key: String,
    connection: MultiplexedConnection,
    session: Arc<GarnetSessionState>,
}

impl ComputationCheckpointStore {
    pub(super) fn new(
        partition: &str,
        connection: MultiplexedConnection,
        session: Arc<GarnetSessionState>,
    ) -> Self {
        Self {
            inner: GarnetCheckpointStore::new(partition, connection.clone(), session.clone()),
            result_sequence_key: format!("ss:{{{partition}}}:result_seq"),
            connection,
            session,
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

    async fn write_result_sequence(
        &self,
        _query_id: &str,
        sequence: u64,
    ) -> Result<(), IndexError> {
        let mut guard = self.session.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "computation result-sequence staging requires an active session",
            ))
        })?;
        buffer.string_set(
            self.result_sequence_key.clone(),
            sequence.to_string().into_bytes(),
        );
        Ok(())
    }

    async fn read_result_sequence(&self, _query_id: &str) -> Result<Option<u64>, IndexError> {
        let buffered = {
            let guard = self.session.lock()?;
            match guard.as_ref() {
                Some(buffer) => buffer.string_get(&self.result_sequence_key),
                None => BufferReadResult::NotInBuffer,
            }
        };
        match buffered {
            BufferReadResult::Found(bytes) => Ok(Some(
                String::from_utf8(bytes)
                    .map_err(IndexError::other)?
                    .parse()
                    .map_err(IndexError::other)?,
            )),
            BufferReadResult::KeyDeleted => Ok(None),
            BufferReadResult::NotInBuffer => self
                .connection
                .clone()
                .get(&self.result_sequence_key)
                .await
                .map_err(IndexError::other),
        }
    }
}
