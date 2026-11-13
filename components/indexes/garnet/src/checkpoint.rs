// Copyright 2024 The Drasi Authors.
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

//! Garnet/Redis implementation of [`CheckpointStore`].
//!
//! Uses Redis string keys (hash-tagged for cluster compatibility) to store
//! per-source checkpoint sequences, opaque source position bytes, and a
//! config hash.
//!
//! Key layout (all hash-tagged to `{query_id}`):
//! - `ss:{<query_id>}:seq:<source_id>` → decimal u64 sequence
//! - `ss:{<query_id>}:pos:<source_id>` → raw opaque bytes
//! - `ss:{<query_id>}:sources` → JSON array of source IDs with checkpoints
//! - `ss:{<query_id>}:config_hash` → decimal u64 hash
//! - `ss:{<query_id>}:result_seq` → last committed result sequence
//! - `ss:{<query_id>}:output_gen` → durable output generation
//!
//! Source and result-sequence staging share the active `GarnetSessionState`
//! buffer. Result-head reads only expose committed data. Clearing bootstrap
//! checkpoints preserves the independently managed result head and generation.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    interface::{
        CheckpointKey, CheckpointStore, IndexError, OutputPartition, SourceCheckpoint, StorageScope,
    },
    ComponentId,
};
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::output_scope::OutputScope;
use crate::session_state::{BufferReadResult, GarnetSessionState};

/// Garnet/Redis-backed checkpoint store.
///
/// Shares a `GarnetSessionState` with the result/element/future indexes so that
/// `stage_checkpoint` writes land in the same buffered transaction as index updates.
pub struct GarnetCheckpointStore {
    scope: OutputScope,
    connection: MultiplexedConnection,
    session_state: Arc<GarnetSessionState>,
}

impl GarnetCheckpointStore {
    pub fn new(
        scope: &StorageScope,
        component_id: &ComponentId,
        connection: MultiplexedConnection,
        session_state: Arc<GarnetSessionState>,
    ) -> Self {
        Self {
            scope: OutputScope::new(scope, component_id),
            connection,
            session_state,
        }
    }

    fn seq_key(&self, key: &CheckpointKey) -> Result<String, IndexError> {
        Ok(format!(
            "ss:{{{}}}:seq:{}",
            self.scope.storage_key(),
            key.storage_key().map_err(IndexError::other)?
        ))
    }

    fn pos_key(&self, key: &CheckpointKey) -> Result<String, IndexError> {
        Ok(format!(
            "ss:{{{}}}:pos:{}",
            self.scope.storage_key(),
            key.storage_key().map_err(IndexError::other)?
        ))
    }

    fn sources_key(&self) -> String {
        format!("ss:{{{}}}:sources", self.scope.storage_key())
    }

    fn config_hash_key(&self) -> String {
        format!("ss:{{{}}}:config_hash", self.scope.storage_key())
    }

    fn result_sequence_key(&self, output: &OutputPartition) -> Result<String, IndexError> {
        output.validate()?;
        Ok(format!("{}:result_seq", self.scope.key("ss", output)))
    }

    fn output_generation_key(&self, output: &OutputPartition) -> Result<String, IndexError> {
        output.validate()?;
        Ok(format!("{}:output_gen", self.scope.key("ss", output)))
    }
}

#[async_trait]
impl CheckpointStore for GarnetCheckpointStore {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &CheckpointKey,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let sources_key = self.sources_key();

        // Pre-fetch sources list from Redis in case buffer doesn't have it
        let redis_list: Option<String> = {
            let mut con = self.connection.clone();
            con.get::<&str, Option<String>>(&sources_key)
                .await
                .map_err(IndexError::other)?
        };

        let mut guard = self.session_state.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "stage_checkpoint requires an active session",
            ))
        })?;

        // Write sequence
        buffer.string_set(self.seq_key(source_id)?, sequence.to_string().into_bytes());

        // Write or delete position
        let pos_key = self.pos_key(source_id)?;
        match source_position {
            Some(pos) => {
                buffer.string_set(pos_key, pos.to_vec());
            }
            None => {
                buffer.del(pos_key);
            }
        }

        // Update sources list
        let current_list = match buffer.string_get(&sources_key) {
            BufferReadResult::Found(bytes) => {
                Some(String::from_utf8(bytes).map_err(IndexError::other)?)
            }
            BufferReadResult::KeyDeleted => None,
            BufferReadResult::NotInBuffer => redis_list,
        };
        let mut source_ids: Vec<CheckpointKey> = match current_list {
            Some(ref s) => serde_json::from_str(s).map_err(IndexError::other)?,
            None => Vec::new(),
        };

        // Always keep source in the sources list for checkpoint tracking
        if !source_ids.contains(source_id) {
            source_ids.push(source_id.clone());
        }

        let serialized = serde_json::to_string(&source_ids).map_err(IndexError::other)?;
        buffer.string_set(sources_key, serialized.into_bytes());

        Ok(())
    }

    async fn read_checkpoint(
        &self,
        source_id: &CheckpointKey,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        let seq_key = self.seq_key(source_id)?;
        let pos_key = self.pos_key(source_id)?;

        // Try buffer first, fall through to Redis
        let (seq_val, pos_val) = {
            let guard = self.session_state.lock()?;
            match guard.as_ref() {
                Some(buffer) => {
                    let seq_val = buffer.string_get(&seq_key);
                    let pos_val = buffer.string_get(&pos_key);
                    (seq_val, pos_val)
                }
                None => (BufferReadResult::NotInBuffer, BufferReadResult::NotInBuffer),
            }
        };

        let sequence = match seq_val {
            BufferReadResult::Found(bytes) => {
                let s = String::from_utf8(bytes).map_err(IndexError::other)?;
                Some(s.parse::<u64>().map_err(IndexError::other)?)
            }
            BufferReadResult::KeyDeleted => return Ok(None),
            BufferReadResult::NotInBuffer => {
                let mut con = self.connection.clone();
                con.get::<String, Option<u64>>(seq_key)
                    .await
                    .map_err(IndexError::other)?
            }
        };

        let sequence = match sequence {
            Some(s) => s,
            None => return Ok(None),
        };

        let source_position = match pos_val {
            BufferReadResult::Found(bytes) => Some(Bytes::from(bytes)),
            BufferReadResult::KeyDeleted => None,
            BufferReadResult::NotInBuffer => {
                let mut con = self.connection.clone();
                match con.get::<String, Option<Vec<u8>>>(pos_key).await {
                    Ok(Some(v)) => Some(Bytes::from(v)),
                    Ok(None) => None,
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        };

        Ok(Some(SourceCheckpoint {
            sequence,
            source_position,
        }))
    }

    async fn read_all_checkpoints(
        &self,
    ) -> Result<HashMap<CheckpointKey, SourceCheckpoint>, IndexError> {
        let sources_key = self.sources_key();

        // Try buffer first for sources list
        let sources_from_buffer = {
            let guard = self.session_state.lock()?;
            match guard.as_ref() {
                Some(buffer) => buffer.string_get(&sources_key),
                None => BufferReadResult::NotInBuffer,
            }
        };

        let source_ids: Vec<CheckpointKey> = match sources_from_buffer {
            BufferReadResult::Found(bytes) => {
                let s = String::from_utf8(bytes).map_err(IndexError::other)?;
                serde_json::from_str(&s).map_err(IndexError::other)?
            }
            BufferReadResult::KeyDeleted => Vec::new(),
            BufferReadResult::NotInBuffer => {
                let mut con = self.connection.clone();
                match con.get::<String, Option<String>>(sources_key).await {
                    Ok(Some(s)) => serde_json::from_str(&s).map_err(IndexError::other)?,
                    Ok(None) => Vec::new(),
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        };

        let mut result = HashMap::new();
        for source_id in &source_ids {
            if let Some(cp) = self.read_checkpoint(source_id).await? {
                result.insert(source_id.clone(), cp);
            }
        }

        Ok(result)
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        // Read all sources to know which keys to delete
        let all = self.read_all_checkpoints().await?;
        let mut con = self.connection.clone();

        for source_id in all.keys() {
            let seq_key = self.seq_key(source_id)?;
            let pos_key = self.pos_key(source_id)?;
            con.del::<&str, ()>(&seq_key)
                .await
                .map_err(IndexError::other)?;
            con.del::<&str, ()>(&pos_key)
                .await
                .map_err(IndexError::other)?;
        }

        let sources_key = self.sources_key();
        con.del::<&str, ()>(&sources_key)
            .await
            .map_err(IndexError::other)?;

        let config_hash_key = self.config_hash_key();
        con.del::<&str, ()>(&config_hash_key)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn stage_clear_checkpoints(&self) -> Result<(), IndexError> {
        let checkpoints = self.read_all_checkpoints().await?;
        let keys = checkpoints
            .keys()
            .map(|key| Ok((self.seq_key(key)?, self.pos_key(key)?)))
            .collect::<Result<Vec<_>, IndexError>>()?;
        self.session_state
            .with_active_buffer(|buffer| {
                for (sequence, position) in keys {
                    buffer.del(sequence);
                    buffer.del(position);
                }
                buffer.del(self.sources_key());
                buffer.del(self.config_hash_key());
            })?
            .ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "stage_clear_checkpoints requires an active session",
                ))
            })?;
        Ok(())
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let key = self.config_hash_key();
        let mut con = self.connection.clone();
        con.set::<&str, String, ()>(&key, hash.to_string())
            .await
            .map_err(IndexError::other)?;
        Ok(())
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        let key = self.config_hash_key();
        let mut con = self.connection.clone();
        match con.get::<String, Option<u64>>(key).await {
            Ok(v) => Ok(v),
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn stage_result_sequence(
        &self,
        output: &OutputPartition,
        sequence: u64,
    ) -> Result<(), IndexError> {
        let key = self.result_sequence_key(output)?;
        match self.session_state.with_active_buffer(|buffer| {
            buffer.string_set(key, sequence.to_string().into_bytes());
        })? {
            Some(()) => Ok(()),
            None => Err(IndexError::other(std::io::Error::other(
                "stage_result_sequence requires an active session",
            ))),
        }
    }

    async fn write_result_sequence(
        &self,
        output: &OutputPartition,
        sequence: u64,
    ) -> Result<(), IndexError> {
        let key = self.result_sequence_key(output)?;
        let mut con = self.connection.clone();
        con.set::<&str, String, ()>(&key, sequence.to_string())
            .await
            .map_err(IndexError::other)?;
        Ok(())
    }

    async fn read_result_sequence(
        &self,
        output: &OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
        let key = self.result_sequence_key(output)?;
        let mut con = self.connection.clone();
        match con.get::<String, Option<u64>>(key).await {
            Ok(v) => Ok(v),
            Err(e) => Err(IndexError::other(e)),
        }
    }

    async fn write_output_generation(
        &self,
        output: &OutputPartition,
        generation: u64,
    ) -> Result<(), IndexError> {
        let key = self.output_generation_key(output)?;
        let mut con = self.connection.clone();
        con.set::<&str, String, ()>(&key, generation.to_string())
            .await
            .map_err(IndexError::other)?;
        Ok(())
    }

    async fn read_output_generation(
        &self,
        output: &OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
        let key = self.output_generation_key(output)?;
        let mut con = self.connection.clone();
        match con.get::<String, Option<u64>>(key).await {
            Ok(v) => Ok(v),
            Err(e) => Err(IndexError::other(e)),
        }
    }
}
