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
//! Uses two Redis data structures per query:
//! - Sorted set `outbox:{<query_id>}` for ordered sequence tracking
//!   (member = sequence number as string, score = sequence as f64)
//! - Hash `outbox_data:{<query_id>}` for raw data storage
//!   (field = sequence number as string, value = raw bytes)
//!
//! Keys are hash-tagged (`{<query_id>}`) for Redis Cluster slot compatibility.
//! Binding a logical query ID preserves those primary keys; other IDs use
//! encoded suffixes within the same hash tag. Scores select candidate entries;
//! exact ordering and bounds use the full u64 members, including above 2^53.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::interface::{IndexError, OutboxWriter};
use redis::aio::MultiplexedConnection;
use redis::{cmd, AsyncCommands};

use crate::{
    output_scope::OutputScope,
    session_state::{BufferReadResult, GarnetSessionState, SortedSetDeltas},
};

/// Garnet/Redis-backed outbox writer.
///
/// Stores serialized query results in a sorted set keyed by sequence number.
pub struct GarnetOutboxWriter {
    scope: OutputScope,
    connection: MultiplexedConnection,
    session_state: Option<Arc<GarnetSessionState>>,
}

impl GarnetOutboxWriter {
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

    /// Attach shared session state so `append` stages into the active session
    /// transaction instead of writing Redis directly. Use this when the outbox
    /// write must commit atomically with other index writes; omit it for
    /// standalone/direct writes (tests, wipe/trim).
    pub fn with_session_state(mut self, session_state: Arc<GarnetSessionState>) -> Self {
        self.session_state = Some(session_state);
        self
    }

    /// Redis key for the outbox sorted set (hash-tagged for cluster).
    fn outbox_key(&self, query_id: &str) -> String {
        self.scope.key("outbox", query_id)
    }

    /// Redis key for storing outbox entry data in a hash (score → data mapping).
    /// The sorted set stores sequence numbers; the hash stores the actual data.
    fn data_key(&self, query_id: &str) -> String {
        self.scope.key("outbox_data", query_id)
    }

    fn has_active_session(&self) -> Result<bool, IndexError> {
        match &self.session_state {
            Some(session) => Ok(session.lock()?.is_some()),
            None => Ok(false),
        }
    }

    async fn remove_sequences(
        &self,
        outbox_key: &str,
        data_key: &str,
        sequences: &[String],
        require_session: bool,
    ) -> Result<(), IndexError> {
        if sequences.is_empty() {
            return Ok(());
        }

        if require_session {
            let session_state = self
                .session_state
                .as_ref()
                .ok_or(IndexError::CorruptedData)?;
            session_state.with_active_buffer_required(|buffer| {
                for seq_str in sequences {
                    buffer.zset_remove(outbox_key.to_string(), seq_str.as_bytes().to_vec());
                    buffer.hash_del(data_key.to_string(), seq_str);
                }
            })?;
            return Ok(());
        }

        let mut con = self.connection.clone();
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.zrem(outbox_key, sequences).ignore();
        pipe.hdel(data_key, sequences).ignore();
        pipe.query_async::<_, ()>(&mut con)
            .await
            .map_err(IndexError::other)?;
        Ok(())
    }

    async fn committed_sequences(&self, outbox_key: &str) -> Result<Vec<u64>, IndexError> {
        let mut con = self.connection.clone();
        let raw: Vec<String> = cmd("ZRANGE")
            .arg(outbox_key)
            .arg(0)
            .arg(-1)
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;
        Self::parse_sequences(raw)
    }

    fn parse_sequences(raw: Vec<String>) -> Result<Vec<u64>, IndexError> {
        raw.into_iter()
            .map(|seq_str| {
                seq_str.parse().map_err(|e| {
                    IndexError::other(std::io::Error::other(format!(
                        "Invalid sequence in outbox: {e}"
                    )))
                })
            })
            .collect()
    }

    fn buffer_overlay(&self, outbox_key: &str) -> Result<Option<SortedSetDeltas>, IndexError> {
        let Some(session_state) = &self.session_state else {
            return Ok(None);
        };
        Ok(session_state
            .with_active_buffer(|buffer| match buffer.zset_get_deltas(outbox_key) {
                BufferReadResult::Found(deltas) => Some(deltas),
                BufferReadResult::KeyDeleted => Some(SortedSetDeltas {
                    added: HashMap::new(),
                    removed: HashSet::new(),
                    full_replace: true,
                }),
                BufferReadResult::NotInBuffer => None,
            })?
            .flatten())
    }
}

fn parse_seq(member: &[u8]) -> Option<u64> {
    std::str::from_utf8(member).ok()?.parse().ok()
}

fn merge_sequences(committed: Vec<u64>, overlay: Option<&SortedSetDeltas>) -> Vec<u64> {
    let mut set: BTreeSet<u64> = match overlay {
        Some(deltas) if deltas.full_replace => BTreeSet::new(),
        _ => committed.into_iter().collect(),
    };
    if let Some(deltas) = overlay {
        for member in &deltas.removed {
            if let Some(seq) = parse_seq(member) {
                set.remove(&seq);
            }
        }
        for member in deltas.added.keys() {
            if let Some(seq) = parse_seq(member) {
                set.insert(seq);
            }
        }
    }
    set.into_iter().collect()
}

#[async_trait]
impl OutboxWriter for GarnetOutboxWriter {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        let outbox_key = self.outbox_key(query_id);
        let data_key = self.data_key(query_id);
        let seq_str = sequence.to_string();

        if let Some(session_state) = &self.session_state {
            session_state.with_active_buffer_required(|buffer| {
                buffer.zset_add(
                    outbox_key.clone(),
                    seq_str.as_bytes().to_vec(),
                    sequence as f64,
                );
                buffer.hash_set(data_key.clone(), &seq_str, data.to_vec());
            })?;
            return Ok(());
        }

        let mut con = self.connection.clone();
        let mut pipe = redis::pipe();
        pipe.atomic();
        pipe.zadd(&outbox_key, &seq_str, sequence as f64).ignore();
        pipe.hset(&data_key, &seq_str, data).ignore();
        pipe.query_async::<_, ()>(&mut con)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        if after_sequence == u64::MAX {
            return Ok(Vec::new());
        }
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key(query_id);
        let data_key = self.data_key(query_id);

        // Include score ties: adjacent u64 values can round to the same f64.
        let sequences: Vec<String> = cmd("ZRANGEBYSCORE")
            .arg(&outbox_key)
            .arg(after_sequence as f64)
            .arg("+inf")
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;

        let mut sequences = Self::parse_sequences(sequences)?;
        sequences.retain(|seq| *seq > after_sequence);
        sequences.sort_unstable();

        // Fetch data for each sequence from the hash
        let mut entries = Vec::with_capacity(sequences.len());
        for seq in sequences {
            let seq_str = seq.to_string();
            let data: Option<Vec<u8>> = con
                .hget::<&str, &str, Option<Vec<u8>>>(&data_key, &seq_str)
                .await
                .map_err(IndexError::other)?;
            if let Some(d) = data {
                entries.push((seq, d));
            }
        }

        Ok(entries)
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        let mut con = self.connection.clone();
        let outbox_key = self.outbox_key(query_id);

        let result: Vec<(String, f64)> = cmd("ZREVRANGE")
            .arg(&outbox_key)
            .arg(0)
            .arg(0)
            .arg("WITHSCORES")
            .query_async(&mut con)
            .await
            .map_err(IndexError::other)?;

        match result.first() {
            Some((_, score)) => {
                let tied: Vec<String> = cmd("ZRANGEBYSCORE")
                    .arg(&outbox_key)
                    .arg(score)
                    .arg(score)
                    .query_async(&mut con)
                    .await
                    .map_err(IndexError::other)?;
                Ok(Self::parse_sequences(tied)?.into_iter().max())
            }
            None => Ok(None),
        }
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        let outbox_key = self.outbox_key(query_id);
        let data_key = self.data_key(query_id);

        if let Some(session) = &self.session_state {
            if session
                .with_active_buffer(|buffer| {
                    buffer.del(outbox_key.clone());
                    buffer.del(data_key.clone());
                })?
                .is_some()
            {
                return Ok(());
            }
        }

        let mut con = self.connection.clone();
        cmd("DEL")
            .arg(&outbox_key)
            .arg(&data_key)
            .query_async::<_, ()>(&mut con)
            .await
            .map_err(IndexError::other)?;

        Ok(())
    }

    async fn trim_before(&self, query_id: &str, retain_from: u64) -> Result<usize, IndexError> {
        let require_session = self.has_active_session()?;
        let outbox_key = self.outbox_key(query_id);
        let data_key = self.data_key(query_id);
        let committed = self.committed_sequences(&outbox_key).await?;
        let overlay = self.buffer_overlay(&outbox_key)?;
        let sequences_to_remove: Vec<String> = merge_sequences(committed, overlay.as_ref())
            .into_iter()
            .filter(|seq| *seq < retain_from)
            .map(|seq| seq.to_string())
            .collect();
        let removed = sequences_to_remove.len();
        self.remove_sequences(
            &outbox_key,
            &data_key,
            &sequences_to_remove,
            require_session,
        )
        .await?;
        Ok(removed)
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        let require_session = self.has_active_session()?;
        let outbox_key = self.outbox_key(query_id);
        let data_key = self.data_key(query_id);
        let committed = self.committed_sequences(&outbox_key).await?;
        let overlay = self.buffer_overlay(&outbox_key)?;
        let effective = merge_sequences(committed, overlay.as_ref());
        if effective.len() <= capacity {
            return Ok(0);
        }
        let to_remove = effective.len() - capacity;
        let sequences_to_remove: Vec<String> = effective
            .into_iter()
            .take(to_remove)
            .map(|seq| seq.to_string())
            .collect();
        self.remove_sequences(
            &outbox_key,
            &data_key,
            &sequences_to_remove,
            require_session,
        )
        .await?;
        Ok(to_remove)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(n: u64) -> Vec<u8> {
        n.to_string().into_bytes()
    }

    #[test]
    fn merge_committed_only() {
        assert_eq!(merge_sequences(vec![1, 3, 2], None), vec![1, 2, 3]);
    }

    #[test]
    fn merge_adds_buffered_members() {
        let overlay = SortedSetDeltas {
            added: HashMap::from([(seq(4), 4.0), (seq(5), 5.0)]),
            removed: HashSet::new(),
            full_replace: false,
        };
        assert_eq!(
            merge_sequences(vec![1, 2, 3], Some(&overlay)),
            vec![1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn merge_full_replace_drops_committed() {
        let overlay = SortedSetDeltas {
            added: HashMap::from([(seq(9), 9.0)]),
            removed: HashSet::new(),
            full_replace: true,
        };
        assert_eq!(merge_sequences(vec![1, 2, 3], Some(&overlay)), vec![9]);
    }

    #[test]
    fn merge_removed_drops_committed_and_added() {
        let overlay = SortedSetDeltas {
            added: HashMap::from([(seq(4), 4.0)]),
            removed: HashSet::from([seq(1)]),
            full_replace: false,
        };
        assert_eq!(
            merge_sequences(vec![1, 2, 3], Some(&overlay)),
            vec![2, 3, 4]
        );
    }
}
