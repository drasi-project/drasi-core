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

#![allow(clippy::unwrap_used)]
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
    collections::{BTreeMap, HashMap, HashSet},
    hash::{Hash, Hasher},
    ops::Bound,
    sync::Arc,
};

use async_stream::stream;
use async_trait::async_trait;
use hashers::jenkins::spooky_hash::SpookyHasher;
use tokio::sync::RwLock;

use crate::{
    interface::{ElementArchiveIndex, ElementIndex, ElementResult, ElementStream, IndexError},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp,
        ElementValue, QueryJoin, QueryJoinKey, TimestampBound, TimestampRange,
    },
    path_solver::match_path::MatchPath,
};

#[allow(clippy::type_complexity)]
pub struct InMemoryElementIndex {
    elements: Arc<RwLock<HashMap<ElementReference, Arc<Element>>>>,

    slot_affinity: Arc<RwLock<HashMap<ElementReference, HashSet<usize>>>>,

    element_by_slot: Arc<RwLock<HashSet<(usize, ElementReference)>>>,
    element_by_slot_in: Arc<RwLock<HashMap<(usize, ElementReference), HashSet<ElementReference>>>>,
    element_by_slot_out: Arc<RwLock<HashMap<(usize, ElementReference), HashSet<ElementReference>>>>,

    element_archive: Arc<RwLock<HashMap<ElementReference, ElementArchive>>>,
    archive_enabled: bool,

    join_spec_by_label: Arc<RwLock<HashMap<Arc<str>, Vec<(Arc<QueryJoin>, Vec<usize>)>>>>,

    // [(join_label, field_value)] => [QueryJoinKey] => ElementReference[]
    partial_joins:
        Arc<RwLock<HashMap<(String, u64), HashMap<QueryJoinKey, HashSet<ElementReference>>>>>,
}

impl Default for InMemoryElementIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryElementIndex {
    pub fn new() -> Self {
        Self {
            elements: Arc::new(RwLock::new(HashMap::new())),
            slot_affinity: Arc::new(RwLock::new(HashMap::new())),
            element_by_slot: Arc::new(RwLock::new(HashSet::new())),
            element_by_slot_in: Arc::new(RwLock::new(HashMap::new())),
            element_by_slot_out: Arc::new(RwLock::new(HashMap::new())),
            element_archive: Arc::new(RwLock::new(HashMap::new())),
            join_spec_by_label: Arc::new(RwLock::new(HashMap::new())),
            partial_joins: Arc::new(RwLock::new(HashMap::new())),
            archive_enabled: false,
        }
    }

    pub fn enable_archive(&mut self) {
        self.archive_enabled = true;
    }

    async fn clear_slot_affinity(&self, element: &Element) -> Result<(), IndexError> {
        let mut af_guard = self.slot_affinity.write().await;
        let affinity = af_guard.entry(element.get_reference().clone()).or_default();

        match element {
            Element::Node {
                metadata,
                properties: _,
            } => {
                let mut guard = self.element_by_slot.write().await;

                for old_slot in affinity.drain() {
                    guard.remove(&(old_slot, metadata.reference.clone()));
                }
            }
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties: _,
            } => {
                let mut in_gaurd = self.element_by_slot_in.write().await;
                let mut out_gaurd = self.element_by_slot_out.write().await;

                for old_slot in affinity.drain() {
                    if let Some(set) = in_gaurd.get_mut(&(old_slot, in_node.clone())) {
                        set.remove(&metadata.reference);
                    }

                    if let Some(set) = out_gaurd.get_mut(&(old_slot, out_node.clone())) {
                        set.remove(&metadata.reference);
                    }
                }
            }
        };

        Ok(())
    }

    async fn set_slot_affinity(
        &self,
        element: &Element,
        slots: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let mut af_guard = self.slot_affinity.write().await;
        let affinity = af_guard.entry(element.get_reference().clone()).or_default();

        match element {
            Element::Node {
                metadata,
                properties: _,
            } => {
                let mut guard = self.element_by_slot.write().await;

                for slot in slots {
                    guard.insert((*slot, metadata.reference.clone()));
                    affinity.insert(*slot);
                }
            }
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties: _,
            } => {
                let mut in_gaurd = self.element_by_slot_in.write().await;
                let mut out_gaurd = self.element_by_slot_out.write().await;

                for slot in slots {
                    affinity.insert(*slot);

                    in_gaurd
                        .entry((*slot, in_node.clone()))
                        .or_default()
                        .insert(metadata.reference.clone());

                    out_gaurd
                        .entry((*slot, out_node.clone()))
                        .or_default()
                        .insert(metadata.reference.clone());
                }
            }
        };

        Ok(())
    }

    async fn update_source_joins(
        &self,
        old_element: Option<Arc<Element>>,
        new_element: Arc<Element>,
    ) -> Result<(), IndexError> {
        match new_element.as_ref() {
            Element::Node {
                metadata,
                properties,
            } => {
                let join_spec_by_label = self.join_spec_by_label.read().await;
                for (label, joins) in join_spec_by_label.iter() {
                    if !metadata.labels.contains(label) {
                        continue;
                    }

                    for (qj, slots) in joins {
                        for qjk in &qj.keys {
                            if qjk.label.as_str() != label.as_ref() {
                                continue;
                            }

                            match properties.get(&qjk.property) {
                                Some(p) => {
                                    if let Some(old_element) = &old_element {
                                        if let Element::Node {
                                            metadata: _old_metadata,
                                            properties: old_properties,
                                        } = old_element.as_ref()
                                        {
                                            if let Some(old_p) = old_properties.get(&qjk.property) {
                                                if old_p == p {
                                                    continue;
                                                }
                                            }
                                        }
                                    }

                                    let value_hash = get_value_hash(p);
                                    let mut partial_joins_guard = self.partial_joins.write().await;
                                    let partial_joins = partial_joins_guard
                                        .entry((qj.id.clone(), value_hash))
                                        .or_default();

                                    let did_insert = partial_joins
                                        .entry(qjk.clone())
                                        .or_default()
                                        .insert(metadata.reference.clone());

                                    let mut elements_to_set = Vec::new();
                                    let mut values_to_delete = Vec::new();

                                    if did_insert {
                                        //remove old partial joins
                                        if let Some(old_element) = &old_element {
                                            if let Element::Node {
                                                metadata: _old_metadata,
                                                properties: old_properties,
                                            } = old_element.as_ref()
                                            {
                                                if let Some(old_p) =
                                                    old_properties.get(&qjk.property)
                                                {
                                                    values_to_delete.push(old_p);
                                                }
                                            }
                                        }

                                        //find matching counterparts
                                        for qjk2 in &qj.keys {
                                            if qjk == qjk2 {
                                                continue;
                                            }

                                            if let Some(others) = partial_joins.get(qjk2) {
                                                for other in others {
                                                    let in_out = Element::Relation {
                                                        metadata: ElementMetadata {
                                                            reference: get_join_virtual_ref(
                                                                new_element.get_reference(),
                                                                other,
                                                            ),
                                                            labels: Arc::from([Arc::from(
                                                                qj.id.clone(),
                                                            )]),
                                                            effective_from: new_element
                                                                .get_effective_from(),
                                                        },
                                                        in_node: new_element
                                                            .get_reference()
                                                            .clone(),
                                                        out_node: other.clone(),
                                                        properties: ElementPropertyMap::new(),
                                                    };

                                                    let out_in = Element::Relation {
                                                        metadata: ElementMetadata {
                                                            reference: get_join_virtual_ref(
                                                                other,
                                                                new_element.get_reference(),
                                                            ),
                                                            labels: Arc::from([Arc::from(
                                                                qj.id.clone(),
                                                            )]),
                                                            effective_from: new_element
                                                                .get_effective_from(),
                                                        },
                                                        in_node: other.clone(),
                                                        out_node: new_element
                                                            .get_reference()
                                                            .clone(),
                                                        properties: ElementPropertyMap::new(),
                                                    };

                                                    elements_to_set.push((in_out, slots.clone()));
                                                    elements_to_set.push((out_in, slots.clone()));
                                                }
                                            }
                                        }
                                    }

                                    drop(partial_joins_guard);

                                    if let Some(old_element) = &old_element {
                                        for val_to_delete in values_to_delete {
                                            self.delete_source_join(
                                                old_element.get_reference(),
                                                qj,
                                                qjk,
                                                val_to_delete,
                                            )
                                            .await?;
                                        }
                                    }

                                    for (element, slots) in elements_to_set {
                                        self.set_element(&element, &slots).await?;
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

    async fn delete_source_joins(&self, old_element: Arc<Element>) -> Result<(), IndexError> {
        match old_element.as_ref() {
            Element::Node {
                metadata,
                properties,
            } => {
                let join_spec_by_label = self.join_spec_by_label.read().await;
                for (label, joins) in join_spec_by_label.iter() {
                    if !metadata.labels.contains(label) {
                        continue;
                    }

                    for (qj, _slots) in joins {
                        for qjk in &qj.keys {
                            if qjk.label.as_str() != label.as_ref() {
                                continue;
                            }

                            match properties.get(&qjk.property) {
                                Some(p) => {
                                    self.delete_source_join(
                                        old_element.get_reference(),
                                        qj,
                                        qjk,
                                        p,
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
        old_element: &ElementReference,
        query_join: &QueryJoin,
        join_key: &QueryJoinKey,
        value: &ElementValue,
    ) -> Result<(), IndexError> {
        let mut elements_to_delete = Vec::new();
        let value_hash = get_value_hash(value);
        let mut partial_joins_guard = self.partial_joins.write().await;
        let partial_joins = partial_joins_guard
            .entry((query_join.id.clone(), value_hash))
            .or_default();

        let did_remove = partial_joins
            .get_mut(join_key)
            .map(|set| set.remove(old_element))
            .unwrap_or(false);

        if did_remove {
            for qjk2 in &query_join.keys {
                if join_key == qjk2 {
                    continue;
                }

                if let Some(others) = partial_joins.get(qjk2) {
                    for other in others {
                        let in_out = get_join_virtual_ref(old_element, other);
                        let out_in = get_join_virtual_ref(other, old_element);

                        elements_to_delete.push(in_out);
                        elements_to_delete.push(out_in);
                    }
                }
            }
        }

        drop(partial_joins_guard);
        for element_to_delete in elements_to_delete {
            self.delete_element(&element_to_delete).await?;
        }

        Ok(())
    }
}

#[async_trait]
impl ElementIndex for InMemoryElementIndex {
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let guard = self.element_by_slot.read().await;
        if guard.contains(&(slot, element_ref.clone())) {
            let guard = self.elements.read().await;
            match guard.get(element_ref) {
                None => Ok(None),
                Some(element) => Ok(Some(element.clone())),
            }
        } else {
            Ok(None)
        }
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let guard = self.element_by_slot_in.read().await;
        match guard.get(&(slot, inbound_ref.clone())) {
            None => Ok(Box::pin(tokio_stream::empty::<ElementResult>())),
            Some(element_refs) => {
                let mut result = Vec::new();
                let guard = self.elements.read().await;
                for element_ref in element_refs {
                    match guard.get(element_ref) {
                        None => (),
                        Some(element) => result.push(element.clone()),
                    }
                }

                Ok(element_vec_to_stream(result))
            }
        }
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let guard = self.element_by_slot_out.read().await;
        match guard.get(&(slot, outbound_ref.clone())) {
            None => Ok(Box::pin(tokio_stream::empty::<ElementResult>())),
            Some(element_refs) => {
                let mut result = Vec::new();
                let guard = self.elements.read().await;
                for element_ref in element_refs {
                    match guard.get(element_ref) {
                        None => (),
                        Some(element) => result.push(element.clone()),
                    }
                }
                Ok(element_vec_to_stream(result))
            }
        }
    }

    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let guard = self.elements.read().await;
        match guard.get(element_ref) {
            None => Ok(None),
            Some(element) => Ok(Some(element.clone())),
        }
    }

    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let mut guard = self.elements.write().await;

        let new_element = Arc::new(element.clone());
        let old_element = guard.insert(element.get_reference().clone(), new_element.clone());

        drop(guard);

        if let Some(prev) = &old_element {
            self.clear_slot_affinity(prev).await?;
        }
        self.set_slot_affinity(new_element.as_ref(), slot_affinity)
            .await?;
        self.update_source_joins(old_element, new_element.clone())
            .await?;

        if self.archive_enabled {
            let mut guard = self.element_archive.write().await;
            let archive = guard
                .entry(new_element.get_reference().clone())
                .or_insert_with(ElementArchive::new);

            archive.insert(new_element.clone());
        }

        Ok(())
    }

    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        let mut guard = self.elements.write().await;
        let old_element = guard.remove(element_ref);

        drop(guard);
        if let Some(old_element) = old_element {
            self.clear_slot_affinity(&old_element).await?;
            self.delete_source_joins(old_element).await?;
        }

        Ok(())
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let mut guard = self.elements.write().await;
        guard.clear();

        let mut guard = self.slot_affinity.write().await;
        guard.clear();

        let mut guard = self.element_by_slot.write().await;
        guard.clear();

        let mut guard = self.element_by_slot_in.write().await;
        guard.clear();

        let mut guard = self.element_by_slot_out.write().await;
        guard.clear();

        let mut guard = self.partial_joins.write().await;
        guard.clear();

        Ok(())
    }

    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        let joins_by_label = extract_join_spec_by_label(match_path, joins);
        let mut join_spec_by_label = self.join_spec_by_label.write().await;
        join_spec_by_label.clone_from(&joins_by_label);
    }
}

#[async_trait]
impl ElementArchiveIndex for InMemoryElementIndex {
    async fn get_element_as_at(
        &self,
        element_ref: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        if !self.archive_enabled {
            return Err(IndexError::NotSupported);
        }

        let guard = self.element_archive.read().await;
        match guard.get(element_ref) {
            None => Ok(None),
            Some(archive) => match archive.get(time) {
                None => Ok(None),
                Some(element) => Ok(Some(element)),
            },
        }
    }

    async fn get_element_versions(
        &self,
        element_ref: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        if !self.archive_enabled {
            return Err(IndexError::NotSupported);
        }

        let from = range.from;
        let to = range.to;
        let guard = self.element_archive.read().await;
        match guard.get(element_ref) {
            None => Ok(Box::pin(tokio_stream::empty::<ElementResult>())),
            Some(archive) => match from {
                TimestampBound::Included(from) => archive.get_range(from, to),
                TimestampBound::StartFromPrevious(from) => {
                    let initial_start = archive.retrieve_previous_start_timestamp(from).unwrap();
                    archive.get_range(initial_start, to)
                }
            },
        }
    }

    async fn clear(&self) -> Result<(), IndexError> {
        if !self.archive_enabled {
            return Err(IndexError::NotSupported);
        }

        let mut guard = self.element_archive.write().await;
        guard.clear();
        Ok(())
    }
}

struct ElementArchive {
    data: BTreeMap<ElementTimestamp, Arc<Element>>,
}

impl ElementArchive {
    fn new() -> Self {
        ElementArchive {
            data: BTreeMap::new(),
        }
    }

    fn insert(&mut self, element: Arc<Element>) {
        self.data.insert(element.get_effective_from(), element);
    }

    fn retrieve_previous_start_timestamp(
        &self,
        time: ElementTimestamp,
    ) -> Option<ElementTimestamp> {
        match self
            .data
            .range((Bound::Included(&0), Bound::Included(&time)))
            .map(|x| *x.0)
            .next_back()
        {
            None => Some(0),
            Some(v) => Some(v),
        }
    }

    fn get(&self, time: ElementTimestamp) -> Option<Arc<Element>> {
        let mut cur = self.data.range((Bound::Unbounded, Bound::Included(&time)));
        if let Some((_, element)) = cur.next_back() {
            Some(element.clone())
        } else {
            None
        }
    }

    fn get_range(
        &self,
        from: ElementTimestamp,
        to: ElementTimestamp,
    ) -> Result<ElementStream, IndexError> {
        let data: Vec<Arc<Element>> = self
            .data
            .range((Bound::Included(&from), Bound::Included(&to)))
            .map(|x| x.1.clone())
            .collect();

        Ok(element_vec_to_stream(data))
    }
}

fn extract_join_spec_by_label(
    match_path: &MatchPath,
    joins: &Vec<Arc<QueryJoin>>,
) -> HashMap<Arc<str>, Vec<(Arc<QueryJoin>, Vec<usize>)>> {
    let mut result: HashMap<Arc<str>, Vec<(Arc<QueryJoin>, Vec<usize>)>> = HashMap::new();

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
                .entry(Arc::from(jk.label.clone()))
                .or_default()
                .push((join.clone(), slots.clone()));
        }
    }

    result
}

fn get_join_virtual_ref(ref1: &ElementReference, ref2: &ElementReference) -> ElementReference {
    let new_id = format!("{}:{}", ref1.element_id, ref2.element_id);
    ElementReference::new("&join", new_id.as_str())
}

fn element_vec_to_stream(elements: Vec<Arc<Element>>) -> ElementStream {
    Box::pin(stream! {
        for e in elements {
            yield Ok(e);
        }
    })
}

fn get_value_hash(value: &ElementValue) -> u64 {
    let mut hasher = SpookyHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ElementValue, QueryJoin, QueryJoinKey};

    /// Test that delete_source_join does not panic when join_key was never registered.
    #[tokio::test]
    async fn test_delete_source_join_missing_key_no_panic() {
        let index = InMemoryElementIndex::new();

        // Create element reference that was never inserted
        let element_ref = ElementReference::new("test_source", "element_123");

        // Create a join key that was never registered
        let query_join = QueryJoin {
            id: "test_join".to_string(),
            keys: vec![QueryJoinKey {
                label: "TestLabel".to_string(),
                property: "id".to_string(),
            }],
        };

        let join_key = &query_join.keys[0];
        let value = ElementValue::String("test_value".into());

        // This should NOT panic - previously it would panic on unwrap()
        let result = index
            .delete_source_join(&element_ref, &query_join, join_key, &value)
            .await;

        // Should complete successfully (no-op) without panic
        assert!(result.is_ok());
    }

    /// Test that delete_source_join correctly removes an element that was previously registered.
    #[tokio::test]
    async fn test_delete_source_join_registered_key() {
        let index = InMemoryElementIndex::new();

        let element_ref = ElementReference::new("test_source", "element_456");
        let value = ElementValue::String("shared_key".into());
        let value_hash = get_value_hash(&value);

        let query_join = QueryJoin {
            id: "test_join".to_string(),
            keys: vec![QueryJoinKey {
                label: "TestLabel".to_string(),
                property: "shared_id".to_string(),
            }],
        };

        let join_key = &query_join.keys[0];

        // Manually register the element in partial_joins
        {
            let mut guard = index.partial_joins.write().await;
            let partial_joins = guard
                .entry((query_join.id.clone(), value_hash))
                .or_default();
            partial_joins
                .entry(join_key.clone())
                .or_default()
                .insert(element_ref.clone());
        }

        // Verify element was registered
        {
            let guard = index.partial_joins.read().await;
            let partial_joins = guard.get(&(query_join.id.clone(), value_hash)).unwrap();
            assert!(partial_joins.get(join_key).unwrap().contains(&element_ref));
        }

        // Delete should succeed
        let result = index
            .delete_source_join(&element_ref, &query_join, join_key, &value)
            .await;
        assert!(result.is_ok());

        // Verify element was removed
        {
            let guard = index.partial_joins.read().await;
            let partial_joins = guard.get(&(query_join.id.clone(), value_hash)).unwrap();
            assert!(!partial_joins.get(join_key).unwrap().contains(&element_ref));
        }
    }

    /// Test that after delete_source_join on a missing key, the index remains in a valid state.
    #[tokio::test]
    async fn test_delete_source_join_missing_key_index_integrity() {
        let index = InMemoryElementIndex::new();

        // Insert a different element with a different key
        let existing_ref = ElementReference::new("test_source", "existing_element");
        let existing_value = ElementValue::String("existing_key".into());
        let existing_hash = get_value_hash(&existing_value);

        let query_join = QueryJoin {
            id: "integrity_join".to_string(),
            keys: vec![QueryJoinKey {
                label: "Label".to_string(),
                property: "prop".to_string(),
            }],
        };

        let join_key = &query_join.keys[0];

        // Register the existing element
        {
            let mut guard = index.partial_joins.write().await;
            let partial_joins = guard
                .entry((query_join.id.clone(), existing_hash))
                .or_default();
            partial_joins
                .entry(join_key.clone())
                .or_default()
                .insert(existing_ref.clone());
        }

        // Try to delete a different element with a different value hash (missing key)
        let missing_ref = ElementReference::new("test_source", "missing_element");
        let missing_value = ElementValue::String("nonexistent_key".into());

        let result = index
            .delete_source_join(&missing_ref, &query_join, join_key, &missing_value)
            .await;
        assert!(result.is_ok());

        // Verify existing element is still intact
        {
            let guard = index.partial_joins.read().await;
            let partial_joins = guard.get(&(query_join.id.clone(), existing_hash)).unwrap();
            assert!(partial_joins.get(join_key).unwrap().contains(&existing_ref));
        }
    }
}
