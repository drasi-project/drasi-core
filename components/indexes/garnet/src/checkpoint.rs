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

//! Garnet/Redis checkpoint writer for source sequence tracking and config hashing.
//!
//! Implements [`CheckpointWriter`] backed by a single Redis hash per query.
//! `stage_checkpoint` writes go through the shared [`GarnetSessionState`]'s
//! `WriteBuffer` so they land in the same `MULTI/EXEC` pipeline as index
//! updates; all read methods and standalone writes execute directly against
//! a cloned `MultiplexedConnection`, returning committed state only.
//!
//! # Key layout
//!
//! All checkpoint state for a query lives in a single hash keyed
//! `ss:{<query_id>}`, with fields:
//! - `source_sequence:{source_id}` — decimal ASCII u64
//! - `config_hash` — decimal ASCII u64
//!
//! The hash-tag braces (`{<query_id>}`) ensure all checkpoint keys hash to the
//! same Redis Cluster slot as the corresponding `ei:`, `ari:`, and `fqi:` keys
//! for that query, so cross-key operations (and any future `MULTI/EXEC` that
//! spans these CFs) are slot-safe.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{CheckpointWriter, IndexError};
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::session_state::{GarnetSessionState, SessionStateError};

const SOURCE_SEQUENCE_PREFIX: &str = "source_sequence:";
const CONFIG_HASH_FIELD: &str = "config_hash";

/// Garnet implementation of [`CheckpointWriter`].
///
/// Holds a [`MultiplexedConnection`] for direct (non-session) reads and writes
/// (`HGET`/`HSET`/`HGETALL`/`DEL`), and an `Arc<GarnetSessionState>` for staging
/// into the active session's `WriteBuffer`.
pub(crate) struct GarnetCheckpointWriter {
    query_id: Arc<str>,
    connection: MultiplexedConnection,
    session_state: Arc<GarnetSessionState>,
}

impl GarnetCheckpointWriter {
    pub(crate) fn new(
        query_id: impl Into<Arc<str>>,
        connection: MultiplexedConnection,
        session_state: Arc<GarnetSessionState>,
    ) -> Self {
        Self {
            query_id: query_id.into(),
            connection,
            session_state,
        }
    }

    fn ss_key(&self) -> String {
        format!("ss:{{{}}}", self.query_id)
    }
}

fn source_sequence_field(source_id: &str) -> String {
    format!("{SOURCE_SEQUENCE_PREFIX}{source_id}")
}

fn parse_decimal_u64(bytes: &[u8]) -> Result<u64, IndexError> {
    let s = std::str::from_utf8(bytes).map_err(|_| IndexError::CorruptedData)?;
    s.parse::<u64>().map_err(|_| IndexError::CorruptedData)
}

#[async_trait]
impl CheckpointWriter for GarnetCheckpointWriter {
    async fn stage_checkpoint(&self, source_id: &str, sequence: u64) -> Result<(), IndexError> {
        let key = self.ss_key();
        let field = source_sequence_field(source_id);
        let value = sequence.to_string().into_bytes();

        let mut guard = self.session_state.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(SessionStateError(
                "stage_checkpoint requires an active session".to_string(),
            ))
        })?;
        buffer.hash_set(key, &field, value);
        Ok(())
    }

    async fn read_checkpoint(&self, source_id: &str) -> Result<Option<u64>, IndexError> {
        let key = self.ss_key();
        let field = source_sequence_field(source_id);
        let mut con = self.connection.clone();
        let raw: Option<Vec<u8>> = con.hget(&key, &field).await.map_err(IndexError::other)?;
        match raw {
            Some(bytes) => Ok(Some(parse_decimal_u64(&bytes)?)),
            None => Ok(None),
        }
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, u64>, IndexError> {
        // Startup-time read: no session involvement, direct HGETALL.
        let key = self.ss_key();
        let mut con = self.connection.clone();
        let entries: HashMap<String, Vec<u8>> =
            con.hgetall(&key).await.map_err(IndexError::other)?;

        let mut out = HashMap::with_capacity(entries.len());
        for (field, value) in entries {
            if let Some(source_id) = field.strip_prefix(SOURCE_SEQUENCE_PREFIX) {
                let sequence = parse_decimal_u64(&value)?;
                out.insert(source_id.to_string(), sequence);
            }
        }
        Ok(out)
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        // Single DEL of the whole hash — atomic, no MULTI/EXEC needed.
        let key = self.ss_key();
        let mut con = self.connection.clone();
        let _: i64 = con.del(&key).await.map_err(IndexError::other)?;
        Ok(())
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        let key = self.ss_key();
        let mut con = self.connection.clone();
        let _: i64 = con
            .hset(&key, CONFIG_HASH_FIELD, hash.to_string())
            .await
            .map_err(IndexError::other)?;
        Ok(())
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        let key = self.ss_key();
        let mut con = self.connection.clone();
        let raw: Option<Vec<u8>> = con
            .hget(&key, CONFIG_HASH_FIELD)
            .await
            .map_err(IndexError::other)?;
        match raw {
            Some(bytes) => Ok(Some(parse_decimal_u64(&bytes)?)),
            None => Ok(None),
        }
    }
}
