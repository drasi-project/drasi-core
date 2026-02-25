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

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use async_trait::async_trait;
use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, IndexError, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter,
    },
};
use hashers::jenkins::spooky_hash::SpookyHasher;
use ordered_float::OrderedFloat;
use redis::{aio::MultiplexedConnection, AsyncCommands};

use crate::{
    session_state::{BufferReadResult, GarnetSessionState},
    storage_models::StoredValueAccumulator,
    ClearByPattern,
};

/// Redis key structure (hash-tagged for cluster compatibility):
/// ari:{<query_id>}:{set_id} -> {value}
/// ari:{<query_id>}:{set_id} -> [sorted value]
/// ari:{<query_id>}:{set_id}:{value} -> count
pub struct GarnetResultIndex {
    query_id: Arc<str>,
    connection: MultiplexedConnection,
    session_state: Arc<GarnetSessionState>,
}

impl GarnetResultIndex {
    /// Create a new GarnetResultIndex from a shared connection.
    pub fn new(
        query_id: &str,
        connection: MultiplexedConnection,
        session_state: Arc<GarnetSessionState>,
    ) -> Self {
        GarnetResultIndex {
            query_id: Arc::from(query_id),
            connection,
            session_state,
        }
    }
}

#[async_trait]
impl AccumulatorIndex for GarnetResultIndex {
    #[tracing::instrument(name = "ari::get", skip_all, err)]
    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let set_id = get_hash_key(owner, key);
        let ari_key = format!("ari:{{{}}}:{}", self.query_id, set_id);

        // Check buffer first
        {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                match buffer.string_get(&ari_key) {
                    BufferReadResult::Found(bytes) => {
                        let stored: StoredValueAccumulator =
                            redis::from_redis_value(&redis::Value::Data(bytes))
                                .map_err(IndexError::other)?;
                        return Ok(Some(stored.into()));
                    }
                    BufferReadResult::KeyDeleted => return Ok(None),
                    BufferReadResult::NotInBuffer => {} // fall through to Redis
                }
            }
        }

        let mut con = self.connection.clone();
        let result = match con
            .get::<String, Option<StoredValueAccumulator>>(ari_key)
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        match result {
            None => Ok(None),
            Some(v) => Ok(Some(v.into())),
        }
    }

    #[tracing::instrument(name = "ari::set", skip_all, err)]
    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let set_id = get_hash_key(&owner, &key);
        let ari_key = format!("ari:{{{}}}:{}", self.query_id, set_id);

        let mut guard = self.session_state.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "write operation requires an active session",
            ))
        })?;
        match value {
            None => {
                buffer.del(ari_key);
            }
            Some(v) => {
                let stored: StoredValueAccumulator = v.into();
                let bytes = redis::ToRedisArgs::to_redis_args(&stored);
                let b = bytes.into_iter().next().ok_or_else(|| {
                    IndexError::other(std::io::Error::other(
                        "StoredValueAccumulator serialized to empty Redis args",
                    ))
                })?;
                buffer.string_set(ari_key, b);
            }
        }
        Ok(())
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.connection
            .clear(format!("ari:{{{}}}:*", self.query_id))
            .await
    }
}

#[async_trait]
impl LazySortedSetStore for GarnetResultIndex {
    #[tracing::instrument(name = "lss::get_next", skip_all, err)]
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let set_key = format!("ari:{{{}}}:{}", self.query_id, set_id);

        let has_session = { self.session_state.lock()?.is_some() };

        if has_session {
            // Session mode: load all members to merge with buffer deltas.
            // Unlike the non-session path (ZRANGEBYSCORE + LIMIT 1), we cannot
            // use a targeted query because buffer additions/removals may shift
            // which entry is "next". These sets are per-aggregation-group and
            // typically small.
            let mut con = self.connection.clone();
            let redis_members: Vec<(String, f64)> = match con
                .zrange_withscores::<&str, Vec<(String, f64)>>(&set_key, 0, -1)
                .await
            {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };

            // Build merged list in a block so MutexGuard is dropped before any await
            let entries = {
                let guard = self.session_state.lock()?;
                let buffer = guard
                    .as_ref()
                    .ok_or_else(|| IndexError::other(std::io::Error::other("session lost")))?;

                let mut entries: Vec<(String, f64)> = Vec::new();
                match buffer.zset_get_deltas(&set_key) {
                    BufferReadResult::Found(deltas) => {
                        for (member, score) in &redis_members {
                            if !deltas.removed.contains(member.as_bytes()) {
                                if let Some(&new_score) = deltas.added.get(member.as_bytes()) {
                                    entries.push((member.clone(), new_score));
                                } else {
                                    entries.push((member.clone(), *score));
                                }
                            }
                        }
                        // Collect new additions separately to avoid borrow conflict
                        let mut new_entries: Vec<(String, f64)> = Vec::new();
                        for (member_bytes, score) in &deltas.added {
                            if let Ok(member_str) = std::str::from_utf8(member_bytes) {
                                if !entries.iter().any(|(m, _)| m == member_str) {
                                    new_entries.push((member_str.to_string(), *score));
                                }
                            }
                        }
                        entries.extend(new_entries);
                    }
                    BufferReadResult::KeyDeleted => {}
                    BufferReadResult::NotInBuffer => {
                        entries = redis_members;
                    }
                }
                entries
            }; // guard dropped here

            // Sort by score
            let mut entries = entries;
            entries.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            // Find next entry after requested value
            let next = match value {
                Some(v) => entries.into_iter().find(|(_, score)| *score > v.0),
                None => entries.into_iter().next(),
            };

            match next {
                None => Ok(None),
                Some((member, score)) => {
                    let count_key = format!("{set_key}:{member}");
                    let count = self.get_value_count_internal(&count_key).await?;
                    Ok(Some((OrderedFloat(score), count)))
                }
            }
        } else {
            // Non-session path: current behavior
            let mut con = self.connection.clone();
            let next_value = match value {
                Some(value) => {
                    match con
                        .zrangebyscore_limit_withscores::<&str, f64, &str, Vec<(String, f64)>>(
                            &set_key, value.0, "+inf", 1, 1,
                        )
                        .await
                    {
                        Ok(v) => {
                            if v.is_empty() {
                                return Ok(None);
                            }
                            (v[0].0.clone(), v[0].1)
                        }
                        Err(e) => return Err(IndexError::other(e)),
                    }
                }
                None => {
                    match con
                        .zrange_withscores::<&str, Vec<(String, f64)>>(&set_key, 0, 0)
                        .await
                    {
                        Ok(v) => {
                            if v.is_empty() {
                                return Ok(None);
                            }
                            (v[0].0.clone(), v[0].1)
                        }
                        Err(e) => return Err(IndexError::other(e)),
                    }
                }
            };

            let count = match con
                .get::<String, Option<isize>>(format!("{}:{}", set_key, next_value.0))
                .await
            {
                Ok(v) => v.unwrap_or(0),
                Err(e) => return Err(IndexError::other(e)),
            };

            Ok(Some((next_value.1.into(), count)))
        }
    }

    #[tracing::instrument(name = "lss::get_value_count", skip_all, err)]
    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let key = format!("ari:{{{}}}:{}:{}", self.query_id, set_id, value.0);
        self.get_value_count_internal(&key).await
    }

    #[tracing::instrument(name = "lss::increment_value_count", skip_all, err)]
    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        let set_key = format!("ari:{{{}}}:{}", self.query_id, set_id);
        let val_key = format!("{}:{}", set_key, value.0);

        // Read current count: check buffer first, then Redis.
        // No INCR in buffer — always store absolute values.
        //
        // Safety: ContinuousQuery::change_lock serializes all
        // process_source_change calls, so no concurrent buffer
        // mutations can occur between lock/drop/re-lock cycles.
        let buffer_result = {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                match buffer.string_get(&val_key) {
                    BufferReadResult::Found(bytes) => Some(Some(bytes)),
                    BufferReadResult::KeyDeleted => Some(None),
                    BufferReadResult::NotInBuffer => None,
                }
            } else {
                return Err(IndexError::other(std::io::Error::other(
                    "write operation requires an active session",
                )));
            }
        }; // guard dropped here

        let current_count = match buffer_result {
            Some(Some(bytes)) => {
                let s = String::from_utf8(bytes).map_err(IndexError::other)?;
                s.parse::<isize>().map_err(IndexError::other)?
            }
            Some(None) => 0,
            None => {
                let mut con = self.connection.clone();
                match con.get::<&str, Option<isize>>(&val_key).await {
                    Ok(v) => v.unwrap_or(0),
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        };

        let new_count = current_count + delta;

        {
            let mut guard = self.session_state.lock()?;
            let buffer = guard.as_mut().ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "write operation requires an active session",
                ))
            })?;

            if new_count == 0 {
                buffer.del(val_key);
                buffer.zset_remove(set_key, value.0.to_string().into_bytes());
            } else {
                buffer.string_set(val_key, new_count.to_string().into_bytes());
                buffer.zset_add(set_key, value.0.to_string().into_bytes(), value.0);
            }
        }

        Ok(())
    }
}

impl GarnetResultIndex {
    /// Internal helper to get a value count, checking the session buffer first.
    async fn get_value_count_internal(&self, key: &str) -> Result<isize, IndexError> {
        {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                match buffer.string_get(key) {
                    BufferReadResult::Found(bytes) => {
                        let s = String::from_utf8(bytes).map_err(IndexError::other)?;
                        return s.parse::<isize>().map_err(IndexError::other);
                    }
                    BufferReadResult::KeyDeleted => return Ok(0),
                    BufferReadResult::NotInBuffer => {} // fall through
                }
            }
        }

        let mut con = self.connection.clone();
        match con.get::<&str, isize>(key).await {
            Ok(v) => Ok(v),
            Err(e) => Err(IndexError::other(e)),
        }
    }
}

/// Redis key structure (hash-tagged for cluster compatibility):
/// metadata:{<query_id>}:sequence -> {value}
/// metadata:{<query_id>}:source_change_id -> {value}
#[async_trait]
impl ResultSequenceCounter for GarnetResultIndex {
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        let seq_key = format!("metadata:{{{}}}:sequence", self.query_id);
        let scid_key = format!("metadata:{{{}}}:source_change_id", self.query_id);

        let mut guard = self.session_state.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "write operation requires an active session",
            ))
        })?;
        buffer.string_set(seq_key, sequence.to_string().into_bytes());
        buffer.string_set(scid_key, source_change_id.as_bytes().to_vec());
        Ok(())
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let seq_key = format!("metadata:{{{}}}:sequence", self.query_id);
        let scid_key = format!("metadata:{{{}}}:source_change_id", self.query_id);

        // Extract buffer reads into a sync block so MutexGuard is dropped before any await
        let result = {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                let seq_val = buffer.string_get(&seq_key);
                let scid_val = buffer.string_get(&scid_key);
                Some((seq_val, scid_val))
            } else {
                None
            }
        }; // guard dropped here

        if let Some((seq_val, scid_val)) = result {
            let sequence = match seq_val {
                BufferReadResult::Found(bytes) => {
                    let s = String::from_utf8(bytes).map_err(IndexError::other)?;
                    s.parse::<u64>().map_err(IndexError::other)?
                }
                BufferReadResult::KeyDeleted => 0,
                BufferReadResult::NotInBuffer => {
                    return self.get_sequence_from_redis().await;
                }
            };

            let source_change_id = match scid_val {
                BufferReadResult::Found(bytes) => {
                    String::from_utf8(bytes).map_err(IndexError::other)?
                }
                BufferReadResult::KeyDeleted => String::new(),
                BufferReadResult::NotInBuffer => {
                    return self.get_sequence_from_redis().await;
                }
            };

            return Ok(ResultSequence {
                sequence,
                source_change_id: Arc::from(source_change_id),
            });
        }

        self.get_sequence_from_redis().await
    }
}

impl GarnetResultIndex {
    async fn get_sequence_from_redis(&self) -> Result<ResultSequence, IndexError> {
        let mut con = self.connection.clone();

        let sequence = match con
            .get::<String, Option<u64>>(format!("metadata:{{{}}}:sequence", self.query_id))
            .await
        {
            Ok(v) => v.unwrap_or(0),
            Err(e) => return Err(IndexError::other(e)),
        };

        let source_change_id = match con
            .get::<String, Option<String>>(format!(
                "metadata:{{{}}}:source_change_id",
                self.query_id
            ))
            .await
        {
            Ok(v) => v.unwrap_or_default(),
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(ResultSequence {
            sequence,
            source_change_id: Arc::from(source_change_id),
        })
    }
}

impl ResultIndex for GarnetResultIndex {}

fn get_hash_key(owner: &ResultOwner, key: &ResultKey) -> u64 {
    let mut hasher = SpookyHasher::default();
    owner.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}
