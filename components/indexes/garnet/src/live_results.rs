// Copyright 2025 The Drasi Authors.
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

//! Garnet/Redis implementation of [`LiveResultsWriter`].
//!
//! Uses a Redis hash per query for O(1) row access:
//! - Key: `live:{<query_id>}` (hash-tagged for cluster compatibility)
//! - Field: `{row_signature}` (u64 as string)
//! - Value: serialized row data (raw bytes)
//!
//! Logical query bindings preserve the primary key. Other query IDs receive
//! encoded suffixes within the same storage scope and Redis Cluster hash tag.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{IndexError, LiveResultsWriter, RowMutation};
use redis::aio::MultiplexedConnection;
use redis::{cmd, AsyncCommands};

use crate::{output_scope::OutputScope, session_state::GarnetSessionState};

/// Garnet/Redis-backed live results writer.
///
/// Stores serialized row data in a Redis hash keyed by row signature.
pub struct GarnetLiveResultsWriter {
    scope: OutputScope,
    connection: MultiplexedConnection,
    session_state: Option<Arc<GarnetSessionState>>,
}

impl GarnetLiveResultsWriter {
    pub fn new(query_id: &str, connection: MultiplexedConnection) -> Self {
        Self {
            scope: OutputScope::new(query_id),
            connection,
            session_state: None,
        }
    }

    /// Bind the primary logical query ID to the constructor's storage scope.
    /// Other IDs use isolated namespaces without changing the primary keys.
    pub fn with_query_id(mut self, query_id: &str) -> Self {
        self.scope.bind_query_id(query_id);
        self
    }

    /// Attach shared session state so `apply_mutations` stages into the active
    /// session transaction instead of writing Redis directly. Use this when
    /// live-result writes must commit atomically with other index writes; omit
    /// it for standalone/direct writes (tests, wipe).
    pub fn with_session_state(mut self, session_state: Arc<GarnetSessionState>) -> Self {
        self.session_state = Some(session_state);
        self
    }

    /// Redis key for the live results hash (hash-tagged for cluster).
    fn live_key(&self, query_id: &str) -> String {
        self.scope.key("live", query_id)
    }
}

#[async_trait]
impl LiveResultsWriter for GarnetLiveResultsWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        let live_key = self.live_key(query_id);

        if let Some(session_state) = &self.session_state {
            session_state.with_active_buffer_required(|buffer| {
                for m in mutations {
                    let field = m.row_signature.to_string();
                    match m.data {
                        Some(data) => buffer.hash_set(live_key.clone(), &field, data.to_vec()),
                        None => buffer.hash_del(live_key.clone(), &field),
                    }
                }
            })?;
            return Ok(());
        }

        let mut con = self.connection.clone();

        // Use a pipeline for atomic batch operations
        let mut pipe = redis::pipe();
        pipe.atomic();
        for m in mutations {
            let field = m.row_signature.to_string();
            match m.data {
                Some(data) => {
                    pipe.hset(&live_key, &field, data);
                }
                None => {
                    pipe.hdel(&live_key, &field);
                }
            }
        }

        pipe.query_async::<MultiplexedConnection, ()>(&mut con)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let mut con = self.connection.clone();
        let live_key = self.live_key(query_id);

        // HGETALL returns alternating field/value pairs
        let result: Vec<(String, Vec<u8>)> = cmd("HGETALL")
            .arg(&live_key)
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;

        let mut entries = Vec::with_capacity(result.len());
        for (field, value) in result {
            let sig: u64 = field.parse().map_err(|e| {
                IndexError::other(std::io::Error::other(format!(
                    "Invalid row_signature in live results: {e}"
                )))
            })?;
            entries.push((sig, value));
        }

        Ok(entries)
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let live_key = self.live_key(query_id);
        if let Some(session) = &self.session_state {
            if session
                .with_active_buffer(|buffer| buffer.del(live_key.clone()))?
                .is_some()
            {
                return Ok(());
            }
        }
        let mut con = self.connection.clone();

        con.del::<&str, ()>(&live_key)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        let mut con = self.connection.clone();
        let live_key = self.live_key(query_id);

        let count: usize = con
            .hlen::<&str, usize>(&live_key)
            .await
            .map_err(IndexError::other)?;

        Ok(count)
    }
}
