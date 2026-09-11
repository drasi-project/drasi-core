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

use async_trait::async_trait;
use drasi_core::interface::{IndexError, LiveResultsWriter, RowMutation};
use redis::aio::MultiplexedConnection;
use redis::{cmd, AsyncCommands};

/// Garnet/Redis-backed live results writer.
///
/// Stores serialized row data in a Redis hash keyed by row signature.
pub struct GarnetLiveResultsWriter {
    query_id: String,
    connection: MultiplexedConnection,
}

impl GarnetLiveResultsWriter {
    pub fn new(query_id: &str, connection: MultiplexedConnection) -> Self {
        Self {
            query_id: query_id.to_string(),
            connection,
        }
    }

    /// Redis key for the live results hash (hash-tagged for cluster).
    fn live_key(&self) -> String {
        format!("live:{{{}}}", self.query_id)
    }
}

#[async_trait]
impl LiveResultsWriter for GarnetLiveResultsWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        let _ = query_id;
        let mut con = self.connection.clone();
        let live_key = self.live_key();

        // Use a pipeline for atomic batch operations
        let mut pipe = redis::pipe();
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
        let _ = query_id;
        let mut con = self.connection.clone();
        let live_key = self.live_key();

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
        let _ = query_id;
        let mut con = self.connection.clone();
        let live_key = self.live_key();

        con.del::<&str, ()>(&live_key)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        let _ = query_id;
        let mut con = self.connection.clone();
        let live_key = self.live_key();

        let count: usize = con
            .hlen::<&str, usize>(&live_key)
            .await
            .map_err(IndexError::other)?;

        Ok(count)
    }
}
