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

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::{
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::{ElementReference, ElementTimestamp},
};
use prost::Message;
use redis::{aio::MultiplexedConnection, AsyncCommands, ToRedisArgs};

use crate::{
    session_state::{BufferReadResult, GarnetSessionState},
    storage_models::{StoredFutureElementRef, StoredFutureElementRefWithContext},
    ClearByPattern,
};

/// Redis key structure (hash-tagged for cluster compatibility):
///
/// fqi:{<query_id>} -> {full future_ref}  (sorted set by due_time)
/// fqi:{<query_id>}:{position_in_query}:{group_signature} -> {future_ref}  (set)
pub struct GarnetFutureQueue {
    query_id: Arc<str>,
    connection: MultiplexedConnection,
    session_state: Arc<GarnetSessionState>,
}

impl GarnetFutureQueue {
    /// Create a new GarnetFutureQueue from a shared connection.
    pub fn new(
        query_id: &str,
        connection: MultiplexedConnection,
        session_state: Arc<GarnetSessionState>,
    ) -> Self {
        Self {
            query_id: Arc::from(query_id),
            connection,
            session_state,
        }
    }

    fn get_queue_key(&self) -> String {
        format!("fqi:{{{}}}", self.query_id)
    }

    fn get_index_key(&self, position_in_query: usize, group_signature: u64) -> String {
        format!(
            "fqi:{{{}}}:{}:{}",
            self.query_id, position_in_query, group_signature
        )
    }
}

#[async_trait]
impl FutureQueue for GarnetFutureQueue {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let future_ref = StoredFutureElementRef {
            element_ref: element_ref.into(),
            original_time,
            due_time,
        };

        let index_key = self.get_index_key(position_in_query, group_signature);
        let queue_key = self.get_queue_key();

        let should_push = match push_type {
            PushType::Always => true,
            PushType::IfNotExists => {
                // Check buffer for the index key, then fall through to Redis
                let buffer_result = {
                    let guard = self.session_state.lock()?;
                    let buffer = guard.as_ref().ok_or_else(|| {
                        IndexError::other(std::io::Error::other(
                            "write operation requires an active session",
                        ))
                    })?;
                    match buffer.set_get_deltas(&index_key) {
                        BufferReadResult::Found(deltas) => {
                            if !deltas.added.is_empty() {
                                Some(true) // has members added in-session, exists
                            } else if !deltas.removed.is_empty() || deltas.full_replace {
                                // In-session removals with no additions, or key was
                                // DEL'd then re-added as empty — treat as non-existing.
                                Some(false)
                            } else {
                                None // no in-session changes, check Redis
                            }
                        }
                        BufferReadResult::KeyDeleted => Some(false), // deleted, doesn't exist
                        BufferReadResult::NotInBuffer => None,       // check Redis
                    }
                };

                match buffer_result {
                    Some(exists) => !exists,
                    None => {
                        // Safety: ContinuousQuery::change_lock serializes all
                        // process_source_change calls, so no concurrent buffer
                        // mutations can occur between drop and re-lock.
                        let mut con = self.connection.clone();
                        let exists: bool = match con.exists(&index_key).await {
                            Ok(v) => v,
                            Err(e) => return Err(IndexError::other(e)),
                        };
                        !exists
                    }
                }
            }
            PushType::Overwrite => {
                self.remove(position_in_query, group_signature).await?;
                true
            }
        };

        if should_push {
            let future_ref_bytes = (&future_ref).to_redis_args();
            let context_ref = StoredFutureElementRefWithContext {
                future_ref,
                position_in_query: position_in_query as u32,
                group_signature,
            };
            let context_ref_bytes = context_ref.to_redis_args();

            let mut guard = self.session_state.lock()?;
            let buffer = guard.as_mut().ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "write operation requires an active session",
                ))
            })?;

            // Serialize the members for buffer storage
            let ref_bytes_flat = future_ref_bytes
                .into_iter()
                .next()
                .ok_or_else(|| IndexError::other(std::io::Error::other("empty redis args")))?;
            let ctx_bytes_flat = context_ref_bytes
                .into_iter()
                .next()
                .ok_or_else(|| IndexError::other(std::io::Error::other("empty redis args")))?;
            buffer.set_add(index_key, ref_bytes_flat);
            buffer.zset_add(queue_key, ctx_bytes_flat, due_time as f64);
        }

        Ok(should_push)
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let index_key = self.get_index_key(position_in_query, group_signature);
        let queue_key = self.get_queue_key();

        // Phase 1: Check buffer state (sync block — guard dropped before any await)
        enum NeedRedis {
            Yes,
            No(Vec<StoredFutureElementRef>),
        }
        let need_redis = {
            let guard = self.session_state.lock()?;
            let buffer = guard.as_ref().ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "write operation requires an active session",
                ))
            })?;
            match buffer.set_get_deltas(&index_key) {
                BufferReadResult::Found(deltas) if deltas.full_replace => {
                    // Only buffer additions — no Redis needed
                    let mut members = Vec::new();
                    for added_bytes in &deltas.added {
                        let member = StoredFutureElementRef::decode(added_bytes.as_slice())
                            .map_err(IndexError::other)?;
                        members.push(member);
                    }
                    NeedRedis::No(members)
                }
                BufferReadResult::Found(_) | BufferReadResult::NotInBuffer => NeedRedis::Yes,
                BufferReadResult::KeyDeleted => NeedRedis::No(Vec::new()),
            }
        }; // guard dropped here

        // Phase 2: Optionally fetch from Redis
        let redis_members: Vec<StoredFutureElementRef> = match &need_redis {
            NeedRedis::Yes => {
                let mut con = self.connection.clone();
                match con.smembers(&index_key).await {
                    Ok(v) => v,
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
            NeedRedis::No(_) => Vec::new(),
        };

        // Phase 3: Re-lock, merge, and buffer removals
        let mut guard = self.session_state.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "write operation requires an active session",
            ))
        })?;

        let all_members: Vec<StoredFutureElementRef> = match need_redis {
            NeedRedis::No(members) => members,
            NeedRedis::Yes => {
                let mut members = Vec::new();
                match buffer.set_get_deltas(&index_key) {
                    BufferReadResult::Found(deltas) => {
                        for m in redis_members {
                            let m_bytes = (&m).to_redis_args();
                            let mb = m_bytes.into_iter().next().ok_or_else(|| {
                                IndexError::other(std::io::Error::other("empty redis args"))
                            })?;
                            if !deltas.removed.contains(&mb) {
                                members.push(m);
                            }
                        }
                        for added_bytes in &deltas.added {
                            let member = StoredFutureElementRef::decode(added_bytes.as_slice())
                                .map_err(IndexError::other)?;
                            members.push(member);
                        }
                    }
                    BufferReadResult::KeyDeleted => {}
                    BufferReadResult::NotInBuffer => {
                        members = redis_members;
                    }
                }
                members
            }
        };

        // Remove each member from the queue sorted set
        for m in all_members {
            let ctx = StoredFutureElementRefWithContext {
                future_ref: m,
                position_in_query: position_in_query as u32,
                group_signature,
            };
            let ctx_bytes = ctx.to_redis_args();
            let bytes = ctx_bytes
                .into_iter()
                .next()
                .ok_or_else(|| IndexError::other(std::io::Error::other("empty redis args")))?;
            buffer.zset_remove(queue_key.clone(), bytes);
        }

        // Delete the index key
        buffer.del(index_key);

        Ok(())
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        // Two-phase pop: build candidate list, then sort + select head + buffer removal.
        let mut con = self.connection.clone();
        let queue_key = self.get_queue_key();

        // Fetch all queue members with scores from Redis
        let redis_members: Vec<(StoredFutureElementRefWithContext, f64)> =
            match con.zrangebyscore_withscores(&queue_key, 0, "+inf").await {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };

        let head = {
            let mut guard = self.session_state.lock()?;
            let buffer = guard.as_mut().ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "write operation requires an active session",
                ))
            })?;

            // Phase 1: Build candidate list
            let mut entries: Vec<(StoredFutureElementRefWithContext, f64)> = match buffer
                .zset_get_deltas(&queue_key)
            {
                BufferReadResult::Found(deltas) if deltas.full_replace => {
                    // Key was DEL'd then re-added — only buffer additions
                    let mut v = Vec::new();
                    for (member_bytes, score) in &deltas.added {
                        let member =
                            StoredFutureElementRefWithContext::decode(member_bytes.as_slice())
                                .map_err(IndexError::other)?;
                        v.push((member, *score));
                    }
                    v
                }
                BufferReadResult::Found(deltas) => {
                    // Merge Redis + buffer deltas
                    let mut v: Vec<(StoredFutureElementRefWithContext, f64)> = Vec::new();
                    for (member, score) in redis_members {
                        let member_bytes = member.to_redis_args();
                        let mb = member_bytes.into_iter().next().ok_or_else(|| {
                            IndexError::other(std::io::Error::other("empty redis args"))
                        })?;
                        if !deltas.removed.contains(&mb) {
                            if let Some(&new_score) = deltas.added.get(&mb) {
                                v.push((member, new_score));
                            } else {
                                v.push((member, score));
                            }
                        }
                    }
                    let seen: std::collections::HashSet<Vec<u8>> = v
                        .iter()
                        .filter_map(|(m, _)| m.to_redis_args().into_iter().next())
                        .collect();
                    for (member_bytes, score) in &deltas.added {
                        if !seen.contains(member_bytes) {
                            let member =
                                StoredFutureElementRefWithContext::decode(member_bytes.as_slice())
                                    .map_err(IndexError::other)?;
                            v.push((member, *score));
                        }
                    }
                    v
                }
                BufferReadResult::KeyDeleted => Vec::new(),
                BufferReadResult::NotInBuffer => redis_members,
            };

            // Phase 2: Sort, select head, buffer removal
            entries.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            match entries.into_iter().next() {
                None => None,
                Some((head, _)) => {
                    let index_key =
                        self.get_index_key(head.position_in_query as usize, head.group_signature);
                    let ctx_bytes = head.to_redis_args();
                    let ctx_flat = ctx_bytes.into_iter().next().ok_or_else(|| {
                        IndexError::other(std::io::Error::other("empty redis args"))
                    })?;
                    let ref_bytes = (&head.future_ref).to_redis_args();
                    let ref_flat = ref_bytes.into_iter().next().ok_or_else(|| {
                        IndexError::other(std::io::Error::other("empty redis args"))
                    })?;
                    buffer.zset_remove(queue_key, ctx_flat);
                    buffer.set_remove(index_key, ref_flat);
                    Some(head)
                }
            }
        };

        Ok(head.map(|h| (&h).into()))
    }

    /// peek_due_time runs outside the session. No buffer awareness needed.
    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let mut con = self.connection.clone();
        let result: Vec<(StoredFutureElementRefWithContext, f64)> = match con
            .zrangebyscore_limit_withscores(self.get_queue_key(), 0, "+inf", 0, 1)
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        if result.is_empty() {
            Ok(None)
        } else {
            Ok(Some(result[0].1 as u64))
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        // Delete the main queue sorted set key (not matched by the :* pattern below)
        let mut con = self.connection.clone();
        let _: () = con
            .del(self.get_queue_key())
            .await
            .map_err(IndexError::other)?;
        // Delete secondary index keys
        self.connection
            .clear(format!("fqi:{{{}}}:*", self.query_id))
            .await
    }
}
