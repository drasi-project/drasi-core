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

use std::{collections::HashMap, hash::Hash, sync::Arc};

use async_recursion::async_recursion;
use async_stream::stream;
use async_trait::async_trait;
use bit_set::BitSet;
use drasi_core::{
    interface::{ElementIndex, ElementStream, IndexError},
    models::{Element, ElementReference, QueryJoin, QueryJoinKey},
    path_solver::match_path::MatchPath,
};
use hashers::jenkins::spooky_hash::SpookyHasher;
use prost::Message;
use tokio::sync::RwLock;

use crate::{
    session_state::{BufferReadResult, GarnetSessionState},
    storage_models::{
        StoredElement, StoredElementContainer, StoredElementMetadata, StoredElementReference,
        StoredRelation, StoredValue, StoredValueMap,
    },
    ClearByPattern,
};

use redis::{aio::MultiplexedConnection, AsyncCommands, Pipeline, ToRedisArgs};

mod archive_index;

/// Redis element index store
///
/// Redis key structure (hash-tagged for cluster compatibility):
/// ei:{<query_id>}:{source_id}:{element_id} -> StoredElement              # Element
/// ei:{<query_id>}:$in:{source_id}:{element_id}:{slot} -> [element_ref]   # Inverted index to inbound elements
/// ei:{<query_id>}:$out:{source_id}:{element_id}:{slot} -> [element_ref]  # Inverted index to outbound elements
/// ei:{<query_id>}:$partial:{join_label}:{field_value}:{node_label}:{property} -> [element_ref] # Partial join index to track source joins
#[allow(clippy::type_complexity)]
pub struct GarnetElementIndex {
    query_id: Arc<str>,
    key_formatter: Arc<KeyFormatter>,
    connection: MultiplexedConnection,
    join_spec_by_label: Arc<RwLock<HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>>>>,
    archive_enabled: bool,
    session_state: Arc<GarnetSessionState>,
}

impl GarnetElementIndex {
    /// Create a new GarnetElementIndex from a shared connection.
    pub fn new(
        query_id: &str,
        connection: MultiplexedConnection,
        archive_enabled: bool,
        session_state: Arc<GarnetSessionState>,
    ) -> Self {
        GarnetElementIndex {
            key_formatter: Arc::new(KeyFormatter::new(Arc::from(query_id))),
            query_id: Arc::from(query_id),
            connection,
            join_spec_by_label: Arc::new(RwLock::new(HashMap::new())),
            archive_enabled,
            session_state,
        }
    }

    /// Simulate SADD with return count in session mode.
    ///
    /// Returns `true` if the member was newly inserted, `false` if already present.
    ///
    /// Precedence:
    /// 1. Already in buffer's `added` set → false (idempotent)
    /// 2. In buffer's `removed` set → move to `added`, true
    /// 3. Buffer has key as `Deleted` → add to fresh set, true
    /// 4. Not in buffer → check Redis SISMEMBER, then buffer SADD if not in Redis
    ///
    /// Safety: Dropping and re-acquiring the std::sync::Mutex guard around async Redis
    /// reads is safe because ContinuousQuery::change_lock serializes all
    /// process_source_change calls for a given query.
    async fn buffered_sadd(&self, key: &str, member_bytes: Vec<u8>) -> Result<bool, IndexError> {
        {
            let mut guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_mut() {
                match buffer.set_is_member(key, &member_bytes) {
                    BufferReadResult::Found(true) => return Ok(false), // already added
                    BufferReadResult::Found(false) => {
                        // In removed set — move to added
                        buffer.set_add(key.to_string(), member_bytes);
                        return Ok(true);
                    }
                    BufferReadResult::KeyDeleted => {
                        // Key was DEL'd — add to fresh set
                        buffer.set_add(key.to_string(), member_bytes);
                        return Ok(true);
                    }
                    BufferReadResult::NotInBuffer => {
                        // Fall through to Redis check below
                    }
                }
            } else {
                // No session — shouldn't be called, but return false
                return Ok(false);
            }
        }

        // Drop lock, check Redis, re-lock
        let mut con = self.connection.clone();
        let exists: bool = con
            .sismember(key, &member_bytes)
            .await
            .map_err(IndexError::other)?;

        if exists {
            return Ok(false);
        }

        let mut guard = self.session_state.lock()?;
        if let Some(buffer) = guard.as_mut() {
            buffer.set_add(key.to_string(), member_bytes);
        }
        Ok(true)
    }

    /// Simulate SREM with return count in session mode.
    ///
    /// Returns `true` if the member was actually removed, `false` if not present.
    ///
    /// Safety: Same as buffered_sadd — lock drop around async read is safe due to
    /// ContinuousQuery::change_lock serialization.
    async fn buffered_srem(&self, key: &str, member_bytes: Vec<u8>) -> Result<bool, IndexError> {
        {
            let mut guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_mut() {
                match buffer.set_is_member(key, &member_bytes) {
                    BufferReadResult::Found(true) => {
                        // In added set — remove from added
                        buffer.set_remove(key.to_string(), member_bytes);
                        return Ok(true);
                    }
                    BufferReadResult::Found(false) => {
                        // Already in removed set
                        return Ok(false);
                    }
                    BufferReadResult::KeyDeleted => {
                        // Key was DEL'd — nothing to remove
                        return Ok(false);
                    }
                    BufferReadResult::NotInBuffer => {
                        // Fall through to Redis check below
                    }
                }
            } else {
                return Ok(false);
            }
        }

        // Drop lock, check Redis, re-lock
        let mut con = self.connection.clone();
        let exists: bool = con
            .sismember(key, &member_bytes)
            .await
            .map_err(IndexError::other)?;

        if !exists {
            return Ok(false);
        }

        let mut guard = self.session_state.lock()?;
        if let Some(buffer) = guard.as_mut() {
            buffer.set_remove(key.to_string(), member_bytes);
        }
        Ok(true)
    }

    /// Get all members of a set, merging buffer deltas with Redis.
    ///
    /// Safety: Same as buffered_sadd — lock drop around async read is safe due to
    /// ContinuousQuery::change_lock serialization.
    async fn buffered_smembers_raw(&self, key: &str) -> Result<Vec<Vec<u8>>, IndexError> {
        let deltas_result = {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                match buffer.set_get_deltas(key) {
                    BufferReadResult::Found(deltas) => Some(Some(deltas)),
                    BufferReadResult::KeyDeleted => Some(None), // empty set
                    BufferReadResult::NotInBuffer => None,      // just use Redis
                }
            } else {
                None
            }
        };

        match deltas_result {
            Some(None) => {
                // Key was deleted — empty set
                Ok(Vec::new())
            }
            Some(Some(deltas)) => {
                // Merge with Redis
                let mut con = self.connection.clone();
                let redis_members: Vec<Vec<u8>> =
                    con.smembers(key).await.map_err(IndexError::other)?;

                let mut result: Vec<Vec<u8>> = Vec::new();
                for m in redis_members {
                    if !deltas.removed.contains(&m) {
                        result.push(m);
                    }
                }
                for m in deltas.added {
                    if !result.contains(&m) {
                        result.push(m);
                    }
                }
                Ok(result)
            }
            None => {
                // No buffer info — just use Redis
                let mut con = self.connection.clone();
                let members: Vec<Vec<u8>> = con.smembers(key).await.map_err(IndexError::other)?;
                Ok(members)
            }
        }
    }

    /// Get all members of a set as StoredElementReference, merging buffer deltas with Redis.
    async fn buffered_smembers_refs(
        &self,
        key: &str,
    ) -> Result<Vec<StoredElementReference>, IndexError> {
        let raw = self.buffered_smembers_raw(key).await?;
        let mut result = Vec::new();
        for bytes in raw {
            match redis::from_redis_value::<StoredElementReference>(&redis::Value::Data(bytes)) {
                Ok(r) => result.push(r),
                Err(e) => return Err(IndexError::other(e)),
            }
        }
        Ok(result)
    }

    async fn update_source_joins(
        &self,
        pipeline: &mut Pipeline,
        new_element: &StoredElement,
    ) -> Result<(), IndexError> {
        let has_session = { self.session_state.lock()?.is_some() };

        match new_element {
            StoredElement::Node(n) => {
                let join_spec_by_label = self.join_spec_by_label.read().await;
                for (label, joins) in join_spec_by_label.iter() {
                    if !n.metadata.labels.contains(label) {
                        continue;
                    }

                    for (qj, slots) in joins {
                        for qjk in &qj.keys {
                            if qjk.label != *label {
                                continue;
                            }

                            match n.properties.get(&qjk.property) {
                                Some(new_value) => {
                                    let element_key = self
                                        .key_formatter
                                        .get_stored_element_key(&n.metadata.reference);
                                    let old_element =
                                        self.get_element_internal(&element_key).await?;

                                    if let Some(StoredElement::Node(old)) = &old_element {
                                        if let Some(old_value) = old.properties.get(&qjk.property) {
                                            if old_value == new_value {
                                                continue;
                                            }
                                        }
                                    }

                                    let pj_key = self.key_formatter.get_partial_join_key(
                                        &qj.id,
                                        new_value,
                                        &qjk.label,
                                        &qjk.property,
                                    );

                                    let element_reference = n.metadata.reference.clone();
                                    let ref_bytes = (&element_reference).to_redis_args();
                                    let ref_bytes_flat =
                                        ref_bytes.into_iter().next().ok_or_else(|| {
                                            IndexError::other(std::io::Error::other(
                                                "empty redis args",
                                            ))
                                        })?;

                                    let did_insert = if has_session {
                                        self.buffered_sadd(&pj_key, ref_bytes_flat).await?
                                    } else {
                                        let mut con = self.connection.clone();
                                        match con
                                            .sadd::<String, &StoredElementReference, usize>(
                                                pj_key.clone(),
                                                &element_reference,
                                            )
                                            .await
                                        {
                                            Ok(count) => count > 0,
                                            Err(e) => return Err(IndexError::other(e)),
                                        }
                                    };

                                    if did_insert {
                                        //remove old partial joins
                                        if let Some(old_element) = &old_element {
                                            if let StoredElement::Node(old) = old_element {
                                                if let Some(old_value) =
                                                    old.properties.get(&qjk.property)
                                                {
                                                    self.delete_source_join(
                                                        pipeline,
                                                        old_element.get_reference(),
                                                        qj,
                                                        qjk,
                                                        old_value,
                                                    )
                                                    .await?;
                                                }
                                            }
                                        }

                                        //find matching counterparts
                                        for qjk2 in &qj.keys {
                                            if qjk == qjk2 {
                                                continue;
                                            }

                                            let other_pj_key =
                                                self.key_formatter.get_partial_join_key(
                                                    &qj.id,
                                                    new_value,
                                                    &qjk2.label,
                                                    &qjk2.property,
                                                );

                                            let others = if has_session {
                                                self.buffered_smembers_refs(&other_pj_key).await?
                                            } else {
                                                let mut con = self.connection.clone();
                                                let mut scan_results = Vec::new();
                                                let mut scan = match con
                                                    .sscan::<String, StoredElementReference>(
                                                        other_pj_key,
                                                    )
                                                    .await
                                                {
                                                    Ok(s) => s,
                                                    Err(e) => return Err(IndexError::other(e)),
                                                };
                                                while let Some(other) = scan.next_item().await {
                                                    scan_results.push(other);
                                                }
                                                scan_results
                                            };

                                            for other in others {
                                                let in_out =
                                                    StoredElement::Relation(StoredRelation {
                                                        metadata: StoredElementMetadata {
                                                            reference: get_join_virtual_ref(
                                                                &element_reference,
                                                                &other,
                                                            ),
                                                            labels: vec![qj.id.clone()],
                                                            effective_from: n
                                                                .metadata
                                                                .effective_from,
                                                        },
                                                        in_node: element_reference.clone(),
                                                        out_node: other.clone(),
                                                        properties: StoredValueMap::new(),
                                                    });

                                                let out_in =
                                                    StoredElement::Relation(StoredRelation {
                                                        metadata: StoredElementMetadata {
                                                            reference: get_join_virtual_ref(
                                                                &other,
                                                                &element_reference,
                                                            ),
                                                            labels: vec![qj.id.clone()],
                                                            effective_from: n
                                                                .metadata
                                                                .effective_from,
                                                        },
                                                        in_node: other.clone(),
                                                        out_node: element_reference.clone(),
                                                        properties: StoredValueMap::new(),
                                                    });

                                                self.set_element_internal(pipeline, in_out, slots)
                                                    .await?;
                                                self.set_element_internal(pipeline, out_in, slots)
                                                    .await?;
                                            }
                                        }
                                    }
                                }
                                None => continue,
                            }
                        }
                    }
                }
            }
            _ => return Ok(()),
        }

        Ok(())
    }

    async fn delete_source_joins(
        &self,
        pipeline: &mut Pipeline,
        old_element: &StoredElement,
    ) -> Result<(), IndexError> {
        match old_element {
            StoredElement::Node(n) => {
                let join_spec_by_label = self.join_spec_by_label.read().await;
                for (label, joins) in join_spec_by_label.iter() {
                    if !n.metadata.labels.contains(label) {
                        continue;
                    }

                    for (qj, _slots) in joins {
                        for qjk in &qj.keys {
                            if qjk.label != *label {
                                continue;
                            }

                            match n.properties.get(&qjk.property) {
                                Some(value) => {
                                    self.delete_source_join(
                                        pipeline,
                                        old_element.get_reference(),
                                        qj,
                                        qjk,
                                        value,
                                    )
                                    .await?;
                                }
                                None => continue,
                            }
                        }
                    }
                }
            }
            _ => return Ok(()),
        }

        Ok(())
    }

    async fn delete_source_join(
        &self,
        pipeline: &mut Pipeline,
        old_element: &StoredElementReference,
        query_join: &QueryJoin,
        join_key: &QueryJoinKey,
        value: &StoredValue,
    ) -> Result<(), IndexError> {
        let has_session = { self.session_state.lock()?.is_some() };

        let pj_key = self.key_formatter.get_partial_join_key(
            &query_join.id,
            value,
            &join_key.label,
            &join_key.property,
        );

        let ref_bytes = old_element.to_redis_args();
        let ref_bytes_flat = ref_bytes
            .into_iter()
            .next()
            .ok_or_else(|| IndexError::other(std::io::Error::other("empty redis args")))?;

        let did_remove = if has_session {
            self.buffered_srem(&pj_key, ref_bytes_flat).await?
        } else {
            let mut con = self.connection.clone();
            match con
                .srem::<&str, &StoredElementReference, usize>(&pj_key, old_element)
                .await
            {
                Ok(count) => count > 0,
                Err(e) => return Err(IndexError::other(e)),
            }
        };

        if did_remove {
            for qjk2 in &query_join.keys {
                if join_key == qjk2 {
                    continue;
                }

                let other_pj_key = self.key_formatter.get_partial_join_key(
                    &query_join.id,
                    value,
                    &qjk2.label,
                    &qjk2.property,
                );

                let others = if has_session {
                    self.buffered_smembers_refs(&other_pj_key).await?
                } else {
                    let mut con = self.connection.clone();
                    let mut scan_results = Vec::new();
                    let mut scan = match con
                        .sscan::<String, StoredElementReference>(other_pj_key)
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => return Err(IndexError::other(e)),
                    };
                    while let Some(other) = scan.next_item().await {
                        scan_results.push(other);
                    }
                    scan_results
                };

                for other in others {
                    let in_out = get_join_virtual_ref(old_element, &other);
                    let out_in = get_join_virtual_ref(&other, old_element);

                    self.delete_element_internal(pipeline, &in_out).await?;
                    self.delete_element_internal(pipeline, &out_in).await?;
                }
            }
        }

        Ok(())
    }

    #[allow(clippy::unwrap_used)]
    #[async_recursion]
    async fn set_element_internal(
        &self,
        pipeline: &mut Pipeline,
        element: StoredElement,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let has_session = { self.session_state.lock()?.is_some() };

        let container = StoredElementContainer::new(element);
        let element_as_redis_args = container.to_redis_args();
        let element = container.element.unwrap();
        let eref = element.get_reference();

        let element_key = self.key_formatter.get_stored_element_key(eref);
        let element_ref_string = self.key_formatter.get_stored_element_ref_string(eref);

        // Read prev_slots: check buffer first, then Redis.
        // Extract buffer check into a sync block so MutexGuard is dropped before any await.
        let prev_slots = if has_session {
            let buffer_result = {
                let guard = self.session_state.lock()?;
                guard.as_ref().map(|b| b.hash_get(&element_key, "slots"))
            }; // guard dropped here
            match buffer_result {
                Some(BufferReadResult::Found(bytes)) => Some(BitSet::from_bytes(bytes.as_slice())),
                Some(BufferReadResult::KeyDeleted) => None,
                _ => {
                    // Safety: ContinuousQuery::change_lock serializes access.
                    let mut con = self.connection.clone();
                    match con
                        .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
                        .await
                    {
                        Ok(Some(prev)) => Some(BitSet::from_bytes(prev.as_slice())),
                        Ok(None) => None,
                        Err(e) => return Err(IndexError::other(e)),
                    }
                }
            }
        } else {
            let mut con = self.connection.clone();
            match con
                .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
                .await
            {
                Ok(Some(prev)) => Some(BitSet::from_bytes(prev.as_slice())),
                Ok(None) => None,
                Err(e) => return Err(IndexError::other(e)),
            }
        };

        let new_slots = slots_to_bitset(slot_affinity);
        let slots_bytes = new_slots.clone().into_bit_vec().to_bytes();

        if has_session {
            let mut guard = self.session_state.lock()?;
            let buffer = guard
                .as_mut()
                .ok_or_else(|| IndexError::other(std::io::Error::other("session lost")))?;

            // Flatten element_as_redis_args to a single byte vec
            let element_bytes: Vec<u8> = element_as_redis_args
                .iter()
                .flat_map(|v| v.clone())
                .collect();
            if element_bytes.is_empty() {
                return Err(IndexError::other(std::io::Error::other(
                    "empty element serialization",
                )));
            }
            buffer.hash_set(element_key.clone(), "e", element_bytes);
            buffer.hash_set(
                element_key.clone(),
                "slots",
                slots_bytes
                    .to_redis_args()
                    .into_iter()
                    .next()
                    .ok_or_else(|| IndexError::other(std::io::Error::other("empty redis args")))?,
            );

            if let StoredElement::Relation(rel) = &element {
                let mut slots_changed = true;

                if let Some(ref prev_slots) = prev_slots {
                    if *prev_slots == new_slots {
                        slots_changed = false;
                    }

                    if slots_changed {
                        for slot in prev_slots.into_iter() {
                            let inbound_key = self
                                .key_formatter
                                .get_stored_inbound_key(&rel.in_node, slot);
                            let outbound_key = self
                                .key_formatter
                                .get_stored_outbound_key(&rel.out_node, slot);

                            buffer.set_remove(inbound_key, element_ref_string.as_bytes().to_vec());
                            buffer.set_remove(outbound_key, element_ref_string.as_bytes().to_vec());
                        }
                    }
                }

                if slots_changed {
                    for slot in slot_affinity {
                        let inbound_key = self
                            .key_formatter
                            .get_stored_inbound_key(&rel.in_node, *slot);
                        let outbound_key = self
                            .key_formatter
                            .get_stored_outbound_key(&rel.out_node, *slot);

                        buffer.set_add(inbound_key, element_ref_string.as_bytes().to_vec());
                        buffer.set_add(outbound_key, element_ref_string.as_bytes().to_vec());
                    }
                }
            }
            drop(guard);
        } else {
            pipeline
                .hset(&element_key, "e", &element_as_redis_args)
                .ignore();
            pipeline
                .hset(&element_key, "slots", slots_bytes.to_redis_args())
                .ignore();

            if let StoredElement::Relation(rel) = &element {
                let mut slots_changed = true;

                if let Some(prev_slots) = prev_slots {
                    if prev_slots == new_slots {
                        slots_changed = false;
                    }

                    if slots_changed {
                        for slot in prev_slots.into_iter() {
                            let inbound_key = self
                                .key_formatter
                                .get_stored_inbound_key(&rel.in_node, slot);
                            let outbound_key = self
                                .key_formatter
                                .get_stored_outbound_key(&rel.out_node, slot);

                            pipeline.srem(&inbound_key, &element_ref_string).ignore();
                            pipeline.srem(&outbound_key, &element_ref_string).ignore();
                        }
                    }
                }

                if slots_changed {
                    for slot in slot_affinity {
                        let inbound_key = self
                            .key_formatter
                            .get_stored_inbound_key(&rel.in_node, *slot);
                        let outbound_key = self
                            .key_formatter
                            .get_stored_outbound_key(&rel.out_node, *slot);

                        pipeline.sadd(&inbound_key, &element_ref_string).ignore();
                        pipeline.sadd(&outbound_key, &element_ref_string).ignore();
                    }
                }
            }
        }

        self.update_source_joins(pipeline, &element).await?;

        if self.archive_enabled {
            self.insert_archive(element.get_metadata(), &element_as_redis_args)
                .await?;
        }

        Ok(())
    }

    async fn get_element_internal(
        &self,
        element_key: &str,
    ) -> Result<Option<StoredElement>, IndexError> {
        // Check buffer first
        {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                match buffer.hash_get(element_key, "e") {
                    BufferReadResult::Found(bytes) => {
                        return match StoredElementContainer::decode(bytes.as_slice()) {
                            Ok(container) => match container.element {
                                Some(element) => Ok(Some(element)),
                                None => Err(IndexError::CorruptedData),
                            },
                            Err(e) => Err(IndexError::other(e)),
                        };
                    }
                    BufferReadResult::KeyDeleted => return Ok(None),
                    BufferReadResult::NotInBuffer => {} // fall through
                }
            }
        }

        let mut con = self.connection.clone();

        let stored: StoredElement = match con
            .hget::<&str, &str, Option<Vec<u8>>>(element_key, "e")
            .await
        {
            Ok(Some(stored)) => match StoredElementContainer::decode(stored.as_slice()) {
                Ok(container) => match container.element {
                    Some(element) => element,
                    None => return Err(IndexError::CorruptedData),
                },
                Err(e) => return Err(IndexError::other(e)),
            },
            Ok(None) => return Ok(None),
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(Some(stored))
    }

    #[async_recursion]
    async fn delete_element_internal(
        &self,
        pipeline: &mut Pipeline,
        element_ref: &StoredElementReference,
    ) -> Result<(), IndexError> {
        let element_key = self.key_formatter.get_stored_element_key(element_ref);
        let has_session = { self.session_state.lock()?.is_some() };

        // Read prev_slots: check buffer first, then Redis.
        // Extract buffer check into sync block so MutexGuard is dropped before any await.
        let prev_slots = if has_session {
            let buffer_result = {
                let guard = self.session_state.lock()?;
                guard.as_ref().map(|b| b.hash_get(&element_key, "slots"))
            }; // guard dropped here
            match buffer_result {
                Some(BufferReadResult::Found(bytes)) => Some(BitSet::from_bytes(bytes.as_slice())),
                Some(BufferReadResult::KeyDeleted) => None,
                _ => {
                    let mut con = self.connection.clone();
                    match con
                        .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
                        .await
                    {
                        Ok(Some(prev)) => Some(BitSet::from_bytes(prev.as_slice())),
                        Ok(None) => None,
                        Err(e) => return Err(IndexError::other(e)),
                    }
                }
            }
        } else {
            let mut con = self.connection.clone();
            match con
                .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
                .await
            {
                Ok(Some(prev)) => Some(BitSet::from_bytes(prev.as_slice())),
                Ok(None) => None,
                Err(e) => return Err(IndexError::other(e)),
            }
        };

        // Read old element BEFORE marking as deleted so get_element_internal
        // can still find it in the buffer/Redis.
        let old_element = self.get_element_internal(element_key.as_str()).await?;

        if has_session {
            let mut guard = self.session_state.lock()?;
            let buffer = guard
                .as_mut()
                .ok_or_else(|| IndexError::other(std::io::Error::other("session lost")))?;
            buffer.del(element_key.clone());

            if let Some(prev_slots) = &prev_slots {
                for slot in prev_slots.into_iter() {
                    let inbound_key = self.key_formatter.get_stored_inbound_key(element_ref, slot);
                    let outbound_key = self
                        .key_formatter
                        .get_stored_outbound_key(element_ref, slot);

                    buffer.del(inbound_key);
                    buffer.del(outbound_key);
                }
            }
            drop(guard);
        } else {
            pipeline.del(&element_key).ignore();

            if let Some(prev_slots) = &prev_slots {
                for slot in prev_slots.into_iter() {
                    let inbound_key = self.key_formatter.get_stored_inbound_key(element_ref, slot);
                    let outbound_key = self
                        .key_formatter
                        .get_stored_outbound_key(element_ref, slot);

                    pipeline.del(&inbound_key).ignore();
                    pipeline.del(&outbound_key).ignore();
                }
            }
        }

        if let Some(old_element) = old_element {
            self.delete_source_joins(pipeline, &old_element).await?;
        }

        Ok(())
    }

    /// Eagerly collect all elements from a set (inbound or outbound),
    /// merging buffer deltas with Redis, and return as a stream.
    async fn get_slot_elements_eager(
        &self,
        set_key: &str,
    ) -> Result<Vec<Arc<Element>>, IndexError> {
        // Get all member ref strings from the set, merging buffer deltas
        let members_raw = self.buffered_smembers_raw(set_key).await?;

        let mut results = Vec::new();
        for member_bytes in members_raw {
            let element_ref = match String::from_utf8(member_bytes) {
                Ok(s) => s,
                Err(e) => return Err(IndexError::other(e)),
            };
            let element_key = self
                .key_formatter
                .get_element_key_from_ref_string(&element_ref);

            match self.get_element_internal(&element_key).await? {
                Some(stored) => {
                    let element: Element = stored.into();
                    results.push(Arc::new(element));
                }
                None => {
                    log::debug!("Garbage collecting reference of deleted element: {element_ref}");
                    // In session mode, we can remove the stale reference from the buffer
                    let mut guard = self.session_state.lock()?;
                    if let Some(buffer) = guard.as_mut() {
                        buffer.set_remove(set_key.to_string(), element_ref.as_bytes().to_vec());
                    }
                }
            }
        }

        Ok(results)
    }
}

#[async_trait]
impl ElementIndex for GarnetElementIndex {
    #[tracing::instrument(skip_all, err)]
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let element_key = self.key_formatter.get_element_key(element_ref);
        let stored = match self.get_element_internal(&element_key).await? {
            Some(stored) => stored,
            None => return Ok(None),
        };

        let element: Element = stored.into();
        Ok(Some(Arc::new(element)))
    }

    #[tracing::instrument(skip_all, err)]
    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let stored: StoredElement = element.into();
        let has_session = { self.session_state.lock()?.is_some() };

        let mut pipeline = redis::pipe();
        if !has_session {
            pipeline.atomic();
        }

        self.set_element_internal(&mut pipeline, stored, slot_affinity)
            .await?;

        if !has_session {
            let mut con = self.connection.clone();
            if let Err(err) = pipeline.query_async::<_, ()>(&mut con).await {
                return Err(IndexError::other(err));
            }
        }
        // Session mode: all writes went to buffer, pipeline is empty

        Ok(())
    }

    #[tracing::instrument(skip_all, err)]
    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        let stored_element_ref: StoredElementReference = element_ref.into();
        let has_session = { self.session_state.lock()?.is_some() };

        let mut pipeline = redis::pipe();
        if !has_session {
            pipeline.atomic();
        }

        log::debug!("Deleting element: {stored_element_ref:?}");
        self.delete_element_internal(&mut pipeline, &stored_element_ref)
            .await?;

        if !has_session {
            let mut con = self.connection.clone();
            if let Err(err) = pipeline.query_async::<_, ()>(&mut con).await {
                return Err(IndexError::other(err));
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let element_key = self.key_formatter.get_element_key(element_ref);

        // Check buffer first for slots
        let stored_slots = {
            let guard = self.session_state.lock()?;
            if let Some(buffer) = guard.as_ref() {
                match buffer.hash_get(&element_key, "slots") {
                    BufferReadResult::Found(bytes) => Some(BitSet::from_bytes(bytes.as_slice())),
                    BufferReadResult::KeyDeleted => return Ok(None),
                    BufferReadResult::NotInBuffer => None, // fall through to Redis
                }
            } else {
                None
            }
        };

        let stored_slots = match stored_slots {
            Some(s) => s,
            None => {
                let mut con = self.connection.clone();
                match con
                    .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
                    .await
                {
                    Ok(Some(stored)) => BitSet::from_bytes(stored.as_slice()),
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        };

        if !stored_slots.contains(slot) {
            return Ok(None);
        }

        // Get element data (checks buffer first via get_element_internal)
        match self.get_element_internal(&element_key).await? {
            Some(stored) => {
                let element: Element = stored.into();
                Ok(Some(Arc::new(element)))
            }
            None => Ok(None),
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let has_session = { self.session_state.lock()?.is_some() };
        let inbound_key = self.key_formatter.get_inbound_key(inbound_ref, slot);

        if has_session {
            // Eager collection: merge buffer + Redis, return as stream
            let results = self.get_slot_elements_eager(&inbound_key).await?;
            Ok(Box::pin(futures::stream::iter(results.into_iter().map(Ok))))
        } else {
            // Non-session path: current SSCAN-based streaming
            let mut con = self.connection.clone();
            let key_formatter = self.key_formatter.clone();

            Ok(Box::pin(stream! {
                let mut con2 = con.clone();
                match con.sscan::<&str, String>(&inbound_key).await {
                    Ok(mut element_refs) => {
                        while let Some(element_ref) = element_refs.next_item().await {
                            let element_key = key_formatter.get_element_key_from_ref_string(&element_ref);
                            let stored = match con2.hget::<&str, &str, Option<Vec<u8>>>(&element_key, "e").await {
                                Ok(Some(stored)) => match StoredElementContainer::decode(stored.as_slice()) {
                                    Ok(container) => match container.element {
                                        Some(element) => Ok(element),
                                        None => Err(IndexError::CorruptedData),
                                    },
                                    Err(e) => Err(IndexError::other(e)),
                                },
                                Ok(None) => {
                                    log::debug!("Garbage collecting reference of deleted element: {element_ref}");
                                    _ = con2.srem::<&str, &str, ()>(&inbound_key, &element_ref).await;
                                    continue;
                                },
                                Err(e) => Err(IndexError::other(e))
                            };

                            match stored {
                                Ok(stored) => {
                                    let element: Element = stored.into();
                                    yield Ok(Arc::new(element));
                                },
                                Err(e) => yield Err(e),
                            }
                        }
                    },
                    Err(e) => yield Err(IndexError::other(e)),
                };
            }))
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let has_session = { self.session_state.lock()?.is_some() };
        let outbound_key = self.key_formatter.get_outbound_key(outbound_ref, slot);

        if has_session {
            // Eager collection: merge buffer + Redis, return as stream
            let results = self.get_slot_elements_eager(&outbound_key).await?;
            Ok(Box::pin(futures::stream::iter(results.into_iter().map(Ok))))
        } else {
            // Non-session path: current SSCAN-based streaming
            let mut con = self.connection.clone();
            let key_formatter = self.key_formatter.clone();

            Ok(Box::pin(stream! {
                let mut con2 = con.clone();
                match con.sscan::<&str, String>(&outbound_key).await {
                    Ok(mut element_refs) => {
                        while let Some(element_ref) = element_refs.next_item().await {
                            let element_key = key_formatter.get_element_key_from_ref_string(&element_ref);
                            let stored = match con2.hget::<&str, &str, Option<Vec<u8>>>(&element_key, "e").await {
                                Ok(Some(stored)) => match StoredElementContainer::decode(stored.as_slice()) {
                                    Ok(container) => match container.element {
                                        Some(element) => Ok(element),
                                        None => Err(IndexError::CorruptedData),
                                    },
                                    Err(e) => Err(IndexError::other(e)),
                                },
                                Ok(None) => {
                                    log::debug!("Garbage collecting reference of deleted element: {element_ref}");
                                    _ = con2.srem::<&str, &str, ()>(&outbound_key, &element_ref).await;
                                    continue;
                                },
                                Err(e) => Err(IndexError::other(e))
                            };

                            match stored {
                                Ok(stored) => {
                                    let element: Element = stored.into();
                                    yield Ok(Arc::new(element));
                                },
                                Err(e) => yield Err(e),
                            }
                        }
                    },
                    Err(e) => yield Err(IndexError::other(e)),
                };
            }))
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.connection
            .clear(format!("ei:{{{}}}:*", self.query_id))
            .await
    }

    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        let joins_by_label = extract_join_spec_by_label(match_path, joins);
        let mut join_spec_by_label = self.join_spec_by_label.write().await;
        join_spec_by_label.clone_from(&joins_by_label);
    }
}

fn slots_to_bitset(slots: &Vec<usize>) -> BitSet {
    let mut bs = BitSet::new();
    for slot in slots {
        bs.insert(*slot);
    }
    bs
}

struct KeyFormatter {
    query_id: Arc<str>,
}

impl KeyFormatter {
    fn new(query_id: Arc<str>) -> Self {
        KeyFormatter { query_id }
    }

    fn get_element_key(&self, element_ref: &ElementReference) -> String {
        format!(
            "ei:{{{}}}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref()
        )
    }

    fn get_stored_element_key(&self, element_ref: &StoredElementReference) -> String {
        format!(
            "ei:{{{}}}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id,
            element_ref.element_id
        )
    }

    fn get_element_key_from_ref_string(&self, element_ref: &str) -> String {
        format!("ei:{{{}}}:{}", self.query_id.as_ref(), element_ref)
    }

    fn get_inbound_key(&self, element_ref: &ElementReference, slot: usize) -> String {
        format!(
            "ei:{{{}}}:$in:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref(),
            slot
        )
    }

    fn get_outbound_key(&self, element_ref: &ElementReference, slot: usize) -> String {
        format!(
            "ei:{{{}}}:$out:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref(),
            slot
        )
    }

    fn get_stored_inbound_key(&self, element_ref: &StoredElementReference, slot: usize) -> String {
        format!(
            "ei:{{{}}}:$in:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id,
            element_ref.element_id,
            slot
        )
    }

    fn get_stored_outbound_key(&self, element_ref: &StoredElementReference, slot: usize) -> String {
        format!(
            "ei:{{{}}}:$out:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id,
            element_ref.element_id,
            slot
        )
    }

    fn get_stored_element_ref_string(&self, element_ref: &StoredElementReference) -> String {
        format!("{}:{}", element_ref.source_id, element_ref.element_id)
    }

    fn get_partial_join_key(
        &self,
        join_label: &str,
        field_value: &StoredValue,
        label: &str,
        property: &str,
    ) -> String {
        let mut hasher = SpookyHasher::default();
        field_value.hash(&mut hasher);
        let value_hash = hasher.finish128();
        format!(
            "ei:{{{}}}:$partial:{}:{}{}:{}:{}",
            self.query_id.as_ref(),
            join_label,
            value_hash.0,
            value_hash.1,
            label,
            property
        )
    }

    fn get_archive_key(&self, element_ref: &ElementReference) -> String {
        format!(
            "archive:{{{}}}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref()
        )
    }

    fn get_stored_archive_key(&self, element_ref: &StoredElementReference) -> String {
        format!(
            "archive:{{{}}}:{}:{}",
            self.query_id.as_ref(),
            &element_ref.source_id,
            &element_ref.element_id
        )
    }
}

#[allow(clippy::type_complexity)]
fn extract_join_spec_by_label(
    match_path: &MatchPath,
    joins: &Vec<Arc<QueryJoin>>,
) -> HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>> {
    let mut result: HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>> = HashMap::new();

    for join in joins {
        let mut slots = Vec::new();
        for (slot_num, slot) in match_path.slots.iter().enumerate() {
            if slot.spec.labels.contains(&join.id.as_str().into()) {
                slots.push(slot_num);
            }
        }
        if slots.is_empty() {
            continue;
        }

        for jk in &join.keys {
            result
                .entry(jk.label.clone())
                .or_default()
                .push((join.clone(), slots.clone()));
        }
    }

    result
}

fn get_join_virtual_ref(
    ref1: &StoredElementReference,
    ref2: &StoredElementReference,
) -> StoredElementReference {
    let new_id = format!("{}:{}", ref1.element_id, ref2.element_id);
    StoredElementReference {
        source_id: "$join".to_string(),
        element_id: new_id,
    }
}
