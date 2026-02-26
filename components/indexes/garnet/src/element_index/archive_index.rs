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

use async_stream::stream;
use async_trait::async_trait;
use drasi_core::{
    interface::{ElementArchiveIndex, ElementStream, IndexError},
    models::{Element, ElementReference, ElementTimestamp, TimestampBound, TimestampRange},
};
use prost::Message;
use redis::{aio::MultiplexedConnection, cmd, AsyncCommands, ToRedisArgs};

use crate::{
    session_state::{BufferReadResult, GarnetSessionState},
    storage_models::{StoredElement, StoredElementContainer, StoredElementMetadata},
    ClearByPattern,
};

use super::GarnetElementIndex;

/// Redis key structure (hash-tagged for cluster compatibility):
///
/// archive:{<query_id>}:{source_id}:{element_id} -> [element (sorted by ts)]
#[async_trait]
impl ElementArchiveIndex for GarnetElementIndex {
    async fn get_element_as_at(
        &self,
        element_ref: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let key = self.key_formatter.get_archive_key(element_ref);

        // Fetch from Redis
        let mut con = self.connection.clone();
        let mut redis_cmd = cmd("ZRANGE");
        let redis_cmd = redis_cmd.arg(&key);
        let redis_cmd = redis_cmd.arg(time);
        let redis_cmd = redis_cmd.arg("-inf");
        let redis_cmd = redis_cmd.arg("BYSCORE");
        let redis_cmd = redis_cmd.arg("REV");
        let redis_cmd = redis_cmd.arg("LIMIT");
        let redis_cmd = redis_cmd.arg(0);
        let redis_cmd = redis_cmd.arg(1);

        let redis_result = match redis_cmd
            .query_async::<MultiplexedConnection, Vec<Vec<u8>>>(&mut con)
            .await
        {
            Ok(v) => v,
            Err(e) => return Err(IndexError::other(e)),
        };

        // Check buffer for sorted set deltas (extract in a sync block)
        let has_session = { self.session_state.lock()?.is_some() };
        if has_session {
            let deltas_result = {
                let guard = self.session_state.lock()?;
                if let Some(buffer) = guard.as_ref() {
                    match buffer.zset_get_deltas(&key) {
                        BufferReadResult::Found(d) => Some(Some(d)),
                        BufferReadResult::KeyDeleted => Some(None),
                        BufferReadResult::NotInBuffer => None,
                    }
                } else {
                    None
                }
            }; // guard dropped here

            match deltas_result {
                Some(Some(deltas)) => {
                    // Merge: get all Redis elements up to `time`, then apply deltas
                    let mut con2 = self.connection.clone();
                    let all_redis: Vec<(Vec<u8>, f64)> = match con2
                        .zrangebyscore_withscores::<&str, u64, u64, Vec<(Vec<u8>, f64)>>(
                            &key, 0, time,
                        )
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => return Err(IndexError::other(e)),
                    };

                    let mut candidates: Vec<(Vec<u8>, f64)> = Vec::new();
                    for (member, score) in all_redis {
                        if !deltas.removed.contains(&member) {
                            if let Some(&new_score) = deltas.added.get(&member) {
                                candidates.push((member, new_score));
                            } else {
                                candidates.push((member, score));
                            }
                        }
                    }

                    // Add buffer additions with score <= time
                    for (member, score) in &deltas.added {
                        if *score <= time as f64 && !candidates.iter().any(|(m, _)| m == member) {
                            candidates.push((member.clone(), *score));
                        }
                    }

                    // Sort by score descending and take the first (most recent <= time)
                    candidates
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                    return match candidates.first() {
                        None => Ok(None),
                        Some((bytes, _)) => {
                            let stored_element: StoredElement =
                                match StoredElementContainer::decode(bytes.as_slice()) {
                                    Ok(container) => match container.element {
                                        Some(element) => element,
                                        None => return Err(IndexError::CorruptedData),
                                    },
                                    Err(e) => return Err(IndexError::other(e)),
                                };
                            Ok(Some(Arc::new(stored_element.into())))
                        }
                    };
                }
                Some(None) => return Ok(None), // key deleted
                None => {}                     // fall through to use Redis result
            }
        }

        // No session or no buffer info — use Redis result directly
        match redis_result.len() {
            0 => Ok(None),
            _ => {
                let element = match redis_result.first() {
                    Some(v) => v,
                    None => return Ok(None),
                };

                let stored_element: StoredElement =
                    match StoredElementContainer::decode(element.as_slice()) {
                        Ok(container) => match container.element {
                            Some(element) => element,
                            None => return Err(IndexError::CorruptedData),
                        },
                        Err(e) => return Err(IndexError::other(e)),
                    };
                Ok(Some(Arc::new(stored_element.into())))
            }
        }
    }

    async fn get_element_versions(
        &self,
        element_ref: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        let mut con = self.connection.clone();
        let key = self.key_formatter.get_archive_key(element_ref);

        let from = range.from;
        let to = range.to;

        let from_timestamp = match from {
            TimestampBound::Included(from) => from,
            TimestampBound::StartFromPrevious(from) => {
                match self.get_element_as_at(element_ref, from).await {
                    Ok(Some(element)) => element.get_effective_from(),
                    Ok(None) => 0,
                    Err(_e) => return Err(IndexError::CorruptedData),
                }
            }
        };

        let has_session = { self.session_state.lock()?.is_some() };

        if has_session {
            // Eager collection with buffer merge
            let redis_results: Vec<(Vec<u8>, f64)> = con
                .zrangebyscore_withscores::<String, u64, u64, Vec<(Vec<u8>, f64)>>(
                    key.clone(),
                    from_timestamp,
                    to,
                )
                .await
                .map_err(IndexError::other)?;

            // Extract buffer deltas in a sync block so guard is dropped before stream creation
            let candidates = {
                let guard = self.session_state.lock()?;
                let mut candidates: Vec<(Vec<u8>, f64)> = Vec::new();

                if let Some(buffer) = guard.as_ref() {
                    match buffer.zset_get_deltas(&key) {
                        BufferReadResult::Found(deltas) => {
                            for (member, score) in redis_results {
                                if !deltas.removed.contains(&member) {
                                    if let Some(&new_score) = deltas.added.get(&member) {
                                        if new_score >= from_timestamp as f64
                                            && new_score <= to as f64
                                        {
                                            candidates.push((member, new_score));
                                        }
                                    } else {
                                        candidates.push((member, score));
                                    }
                                }
                            }
                            for (member, score) in &deltas.added {
                                if *score >= from_timestamp as f64
                                    && *score <= to as f64
                                    && !candidates.iter().any(|(m, _)| m == member)
                                {
                                    candidates.push((member.clone(), *score));
                                }
                            }
                        }
                        BufferReadResult::KeyDeleted => {}
                        BufferReadResult::NotInBuffer => {
                            candidates = redis_results;
                        }
                    }
                } else {
                    candidates = redis_results;
                }
                candidates
            }; // guard dropped here

            // Sort by score ascending
            let mut candidates = candidates;
            candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

            let mut elements = Vec::new();
            for (bytes, _) in candidates {
                match StoredElementContainer::decode(bytes.as_slice()) {
                    Ok(container) => match container.element {
                        Some(element) => elements.push(Ok(Arc::new(element.into()))),
                        None => elements.push(Err(IndexError::CorruptedData)),
                    },
                    Err(e) => elements.push(Err(IndexError::other(e))),
                }
            }

            Ok(Box::pin(futures::stream::iter(elements)))
        } else {
            // Non-session: current streaming behavior
            let stream = stream! {
                let result = con
                    .zrangebyscore::<String, u64, u64, Vec<Vec<u8>>>(key, from_timestamp, to)
                    .await;

                match result {
                    Ok(result) => {
                        for element in result {
                            match StoredElementContainer::decode(element.as_slice()) {
                                Ok(container) => match container.element {
                                    Some(element) => yield Ok(Arc::new(element.into())),
                                    None => yield Err(IndexError::CorruptedData),
                                }
                                Err(e) => yield Err(IndexError::other(e)),
                            };
                        }
                    }
                    Err(e) => {
                        yield Err(IndexError::other(e));
                    }
                }
            };
            Ok(Box::pin(stream))
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.connection
            .clear(format!("archive:{{{}}}:*", self.query_id))
            .await
    }
}

impl GarnetElementIndex {
    pub async fn insert_archive(
        &self,
        metadata: &StoredElementMetadata,
        element: &Vec<Vec<u8>>,
    ) -> Result<(), IndexError> {
        let key = self
            .key_formatter
            .get_stored_archive_key(&metadata.reference);
        let element_bytes: Vec<u8> = element.iter().flat_map(|v| v.clone()).collect();
        if element_bytes.is_empty() {
            return Err(IndexError::other(std::io::Error::other(
                "empty element serialization",
            )));
        }

        let mut guard = self.session_state.lock()?;
        let buffer = guard.as_mut().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "write operation requires an active session",
            ))
        })?;
        buffer.zset_add(key, element_bytes, metadata.effective_from as f64);
        Ok(())
    }
}
