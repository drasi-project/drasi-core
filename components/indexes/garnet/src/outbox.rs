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

//! Garnet/Redis implementation of [`OutboxWriter`].
//!
//! Uses a Redis sorted set per query for ordered sequence access:
//! - Key: `outbox:{<query_id>}` (hash-tagged for cluster compatibility)
//! - Score: sequence number (u64 as f64)
//! - Member: `{sequence_u64}:{data_bytes}` (sequence prefix for uniqueness)
//!
//! Since sorted set members must be unique and we need to store arbitrary bytes,
//! members are stored as: `{sequence_be_hex}:{base64(data)}`.

use async_trait::async_trait;
use drasi_core::interface::{IndexError, OutboxWriter};
use redis::aio::MultiplexedConnection;
use redis::{cmd, AsyncCommands};

/// Garnet/Redis-backed outbox writer.
///
/// Stores serialized query results in a sorted set keyed by sequence number.
pub struct GarnetOutboxWriter {
    query_id: String,
    connection: MultiplexedConnection,
}

impl GarnetOutboxWriter {
    pub fn new(query_id: &str, connection: MultiplexedConnection) -> Self {
        Self {
            query_id: query_id.to_string(),
            connection,
        }
    }

    /// Redis key for the outbox sorted set (hash-tagged for cluster).
    fn outbox_key(&self) -> String {
        format!("outbox:{{{}}}", self.query_id)
    }

    /// Redis key for storing outbox entry data in a hash (score → data mapping).
    /// The sorted set stores sequence numbers; the hash stores the actual data.
    fn data_key(&self) -> String {
        format!("outbox_data:{{{}}}", self.query_id)
    }
}

#[async_trait]
impl OutboxWriter for GarnetOutboxWriter {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        let _ = query_id; // query_id is already bound in self
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key();
        let data_key = self.data_key();
        let seq_str = sequence.to_string();

        // Add sequence to sorted set (score = sequence for ordering)
        con.zadd::<&str, f64, &str, ()>(&outbox_key, &seq_str, sequence as f64)
            .await
            .map_err(IndexError::other)?;

        // Store data in a hash keyed by sequence
        con.hset::<&str, &str, &[u8], ()>(&data_key, &seq_str, data)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        let _ = query_id;
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key();
        let data_key = self.data_key();

        // ZRANGEBYSCORE to get sequences > after_sequence
        let min = format!("({after_sequence}"); // exclusive lower bound
        let max = "+inf";

        let sequences: Vec<String> = cmd("ZRANGEBYSCORE")
            .arg(&outbox_key)
            .arg(&min)
            .arg(max)
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;

        if sequences.is_empty() {
            return Ok(Vec::new());
        }

        // Fetch data for each sequence from the hash
        let mut entries = Vec::with_capacity(sequences.len());
        for seq_str in &sequences {
            let seq: u64 = seq_str.parse().map_err(|e| {
                IndexError::other(std::io::Error::other(format!(
                    "Invalid sequence in outbox: {e}"
                )))
            })?;
            let data: Option<Vec<u8>> = con
                .hget::<&str, &str, Option<Vec<u8>>>(&data_key, seq_str)
                .await
                .map_err(IndexError::other)?;
            if let Some(d) = data {
                entries.push((seq, d));
            }
        }

        Ok(entries)
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        let _ = query_id;
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key();

        // ZREVRANGE key 0 0 — get the highest-scoring member
        let result: Vec<String> = cmd("ZREVRANGE")
            .arg(&outbox_key)
            .arg(0)
            .arg(0)
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;

        match result.first() {
            Some(seq_str) => {
                let seq: u64 = seq_str.parse().map_err(|e| {
                    IndexError::other(std::io::Error::other(format!(
                        "Invalid sequence in outbox: {e}"
                    )))
                })?;
                Ok(Some(seq))
            }
            None => Ok(None),
        }
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let _ = query_id;
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key();
        let data_key = self.data_key();

        con.del::<&str, ()>(&outbox_key)
            .await
            .map_err(IndexError::other)?;
        con.del::<&str, ()>(&data_key)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        let _ = query_id;
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key();
        let data_key = self.data_key();

        // Get total count
        let total: usize = con
            .zcard::<&str, usize>(&outbox_key)
            .await
            .map_err(IndexError::other)?;

        if total <= capacity {
            return Ok(0);
        }

        let to_remove = total - capacity;

        // Get the sequences to remove (lowest N)
        let sequences_to_remove: Vec<String> = cmd("ZRANGE")
            .arg(&outbox_key)
            .arg(0)
            .arg((to_remove - 1) as isize)
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;

        // Remove from sorted set (by rank)
        con.zremrangebyrank::<&str, ()>(&outbox_key, 0, (to_remove - 1) as isize)
            .await
            .map_err(IndexError::other)?;

        // Remove data entries from hash
        for seq_str in &sequences_to_remove {
            con.hdel::<&str, &str, ()>(&data_key, seq_str)
                .await
                .map_err(IndexError::other)?;
        }

        Ok(to_remove)
    }
}
