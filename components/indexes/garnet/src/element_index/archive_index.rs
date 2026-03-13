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

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use drasi_core::{
    interface::{ElementArchiveIndex, ElementStream, IndexError},
    models::{Element, ElementReference, ElementTimestamp, TimestampBound, TimestampRange},
};
use prost::Message;
use redis::{aio::MultiplexedConnection, cmd, AsyncCommands};

use crate::{
    session_state::BufferReadResult,
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

        // Check buffer first (cheap: mutex + hashmap lookup) to decide
        // whether we need a full scan or can use the optimized single-element query.
        let deltas_result = {
            let guard = self.session_state.lock()?;
            let buffer = guard.as_ref().ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "read operation requires an active session",
                ))
            })?;
            match buffer.zset_get_deltas(&key) {
                BufferReadResult::Found(d) => Some(Some(d)),
                BufferReadResult::KeyDeleted => Some(None),
                BufferReadResult::NotInBuffer => None,
            }
        }; // guard dropped here

        match deltas_result {
            Some(None) => Ok(None), // key deleted in-session
            None => {
                // No buffer deltas — use optimized O(log N) single-element query
                let mut con = self.connection.clone();
                let result: Vec<Vec<u8>> = cmd("ZRANGE")
                    .arg(&key)
                    .arg(time)
                    .arg("-inf")
                    .arg("BYSCORE")
                    .arg("REV")
                    .arg("LIMIT")
                    .arg(0)
                    .arg(1)
                    .query_async(&mut con)
                    .await
                    .map_err(IndexError::other)?;

                match result.first() {
                    None => Ok(None),
                    Some(bytes) => Self::decode_element(bytes),
                }
            }
            Some(Some(deltas)) => {
                // Buffer has deltas — full scan + merge required
                let mut con = self.connection.clone();
                let all_redis: Vec<(Vec<u8>, f64)> = con
                    .zrangebyscore_withscores::<&str, u64, u64, Vec<(Vec<u8>, f64)>>(&key, 0, time)
                    .await
                    .map_err(IndexError::other)?;

                let mut candidates: Vec<(Vec<u8>, f64)> = if deltas.full_replace {
                    deltas
                        .added
                        .into_iter()
                        .filter(|(_, score)| *score <= time as f64)
                        .collect()
                } else {
                    let mut merged: Vec<(Vec<u8>, f64)> = Vec::new();
                    for (member, score) in all_redis {
                        if !deltas.removed.contains(&member) {
                            if let Some(&new_score) = deltas.added.get(&member) {
                                merged.push((member, new_score));
                            } else {
                                merged.push((member, score));
                            }
                        }
                    }

                    let seen: HashSet<Vec<u8>> = merged.iter().map(|(m, _)| m.clone()).collect();

                    for (member, score) in &deltas.added {
                        if *score <= time as f64 && !seen.contains(member) {
                            merged.push((member.clone(), *score));
                        }
                    }
                    merged
                };

                candidates
                    .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                match candidates.first() {
                    None => Ok(None),
                    Some((bytes, _)) => Self::decode_element(bytes),
                }
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
            let buffer = guard.as_ref().ok_or_else(|| {
                IndexError::other(std::io::Error::other(
                    "read operation requires an active session",
                ))
            })?;
            let mut candidates: Vec<(Vec<u8>, f64)> = Vec::new();

            match buffer.zset_get_deltas(&key) {
                BufferReadResult::Found(deltas) => {
                    if deltas.full_replace {
                        // Key was DEL'd then re-added — only use buffer additions
                        for (member, score) in &deltas.added {
                            if *score >= from_timestamp as f64 && *score <= to as f64 {
                                candidates.push((member.clone(), *score));
                            }
                        }
                    } else {
                        for (member, score) in redis_results {
                            if !deltas.removed.contains(&member) {
                                if let Some(&new_score) = deltas.added.get(&member) {
                                    if new_score >= from_timestamp as f64 && new_score <= to as f64
                                    {
                                        candidates.push((member, new_score));
                                    }
                                } else {
                                    candidates.push((member, score));
                                }
                            }
                        }

                        // Collect seen members for O(1) dedup
                        let seen: HashSet<Vec<u8>> =
                            candidates.iter().map(|(m, _)| m.clone()).collect();

                        for (member, score) in &deltas.added {
                            if *score >= from_timestamp as f64
                                && *score <= to as f64
                                && !seen.contains(member)
                            {
                                candidates.push((member.clone(), *score));
                            }
                        }
                    }
                }
                BufferReadResult::KeyDeleted => {}
                BufferReadResult::NotInBuffer => {
                    candidates = redis_results;
                }
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
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.connection
            .clear(format!("archive:{{{}}}:*", self.query_id))
            .await
    }
}

impl GarnetElementIndex {
    fn decode_element(bytes: &[u8]) -> Result<Option<Arc<Element>>, IndexError> {
        let stored_element: StoredElement = match StoredElementContainer::decode(bytes) {
            Ok(container) => match container.element {
                Some(element) => element,
                None => return Err(IndexError::CorruptedData),
            },
            Err(e) => return Err(IndexError::other(e)),
        };
        Ok(Some(Arc::new(stored_element.into())))
    }

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
