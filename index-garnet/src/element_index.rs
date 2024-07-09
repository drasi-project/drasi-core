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
/// Redis key structure:
/// ei:{query_id}:{source_id}:{element_id} -> StoredElement              # Element
/// ei:{query_id}:$in:{source_id}:{element_id}:{slot} -> [element_ref]   # Inverted index to inbound elements
/// ei:{query_id}:$out:{source_id}:{element_id}:{slot} -> [element_ref]  # Inverted index to outbound elements
/// ei:{query_id}:$partial:{join_label}:{field_value}:{node_label}:{property} -> [element_ref] # Partial join index to track source joins
pub struct GarnetElementIndex {
    query_id: Arc<str>,
    key_formatter: Arc<KeyFormatter>,
    connection: MultiplexedConnection,
    join_spec_by_label: Arc<RwLock<HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>>>>,
    archive_enabled: bool,
}

impl GarnetElementIndex {
    pub async fn connect(query_id: &str, url: &str) -> Result<Self, IndexError> {
        let client = match redis::Client::open(url) {
            Ok(client) => client,
            Err(e) => return Err(IndexError::connection_failed(e)),
        };

        let connection = match client.get_multiplexed_async_connection().await {
            Ok(con) => con,
            Err(e) => return Err(IndexError::connection_failed(e)),
        };

        Ok(GarnetElementIndex {
            key_formatter: Arc::new(KeyFormatter::new(Arc::from(query_id))),
            query_id: Arc::from(query_id),
            connection,
            join_spec_by_label: Arc::new(RwLock::new(HashMap::new())),
            archive_enabled: false,
        })
    }

    pub fn enable_archive(&mut self) {
        self.archive_enabled = true;
    }

    async fn update_source_joins(
        &self,
        pipeline: &mut Pipeline,
        new_element: &StoredElement,
    ) -> Result<(), IndexError> {
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
                                    let mut con = self.connection.clone();
                                    let element_key = self
                                        .key_formatter
                                        .get_stored_element_key(&n.metadata.reference);
                                    let old_element =
                                        self.get_element_internal(&element_key).await?;

                                    if let Some(old_element) = &old_element {
                                        if let StoredElement::Node(old) = old_element {
                                            if let Some(old_value) =
                                                old.properties.get(&qjk.property)
                                            {
                                                if old_value == new_value {
                                                    continue;
                                                }
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

                                    let did_insert = match con
                                        .sadd::<String, &StoredElementReference, usize>(
                                            pj_key,
                                            &element_reference,
                                        )
                                        .await
                                    {
                                        Ok(count) => count > 0,
                                        Err(e) => return Err(IndexError::other(e)),
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

                                            let mut others = match con
                                                .sscan::<String, StoredElementReference>(
                                                    other_pj_key,
                                                )
                                                .await
                                            {
                                                Ok(others) => others,
                                                Err(e) => return Err(IndexError::other(e)),
                                            };

                                            while let Some(other) = others.next_item().await {
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
        let mut con = self.connection.clone();

        let pj_key = self.key_formatter.get_partial_join_key(
            &query_join.id,
            value,
            &join_key.label,
            &join_key.property,
        );

        let did_remove = match con
            .srem::<&str, &StoredElementReference, usize>(&pj_key, old_element)
            .await
        {
            Ok(count) => count > 0,
            Err(e) => return Err(IndexError::other(e)),
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

                let mut others = match con
                    .sscan::<String, StoredElementReference>(other_pj_key)
                    .await
                {
                    Ok(others) => others,
                    Err(e) => return Err(IndexError::other(e)),
                };

                while let Some(other) = others.next_item().await {
                    let in_out = get_join_virtual_ref(old_element, &other);
                    let out_in = get_join_virtual_ref(&other, old_element);

                    self.delete_element_internal(pipeline, &in_out).await?;
                    self.delete_element_internal(pipeline, &out_in).await?;
                }
            }
        }

        Ok(())
    }

    #[async_recursion]
    async fn set_element_internal(
        &self,
        pipeline: &mut Pipeline,
        element: StoredElement,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let mut con = self.connection.clone();

        let container = StoredElementContainer::new(element);
        let element_as_redis_args = container.to_redis_args();
        let element = container.element.unwrap();
        let eref = element.get_reference();

        let element_key = self.key_formatter.get_stored_element_key(eref);
        let element_ref_string = self.key_formatter.get_stored_element_ref_string(eref);

        let prev_slots = match con
            .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
            .await
        {
            Ok(Some(prev)) => Some(BitSet::from_bytes(prev.as_slice())),
            Ok(None) => None,
            Err(e) => return Err(IndexError::other(e)),
        };

        let new_slots = slots_to_bitset(slot_affinity);

        pipeline
            .hset(&element_key, "e", &element_as_redis_args)
            .ignore();
        pipeline
            .hset(
                &element_key,
                "slots",
                &new_slots.clone().into_bit_vec().to_bytes().to_redis_args(),
            )
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
        let mut con = self.connection.clone();
        let prev_slots = match con
            .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
            .await
        {
            Ok(Some(prev)) => Some(BitSet::from_bytes(prev.as_slice())),
            Ok(None) => None,
            Err(e) => return Err(IndexError::other(e)),
        };

        pipeline.del(&element_key).ignore();

        if let Some(prev_slots) = prev_slots {
            for slot in prev_slots.into_iter() {
                let inbound_key = self.key_formatter.get_stored_inbound_key(element_ref, slot);
                let outbound_key = self
                    .key_formatter
                    .get_stored_outbound_key(element_ref, slot);

                pipeline.del(&inbound_key).ignore();
                pipeline.del(&outbound_key).ignore();
            }
        }

        match self.get_element_internal(element_key.as_str()).await? {
            Some(old_element) => {
                self.delete_source_joins(pipeline, &old_element).await?;
            }
            None => (),
        }

        Ok(())
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

        let mut pipeline = redis::pipe();
        pipeline.atomic();

        self.set_element_internal(&mut pipeline, stored, slot_affinity)
            .await?;

        let mut con = self.connection.clone();

        if let Err(err) = pipeline.query_async::<_, ()>(&mut con).await {
            return Err(IndexError::other(err));
        }

        Ok(())
    }

    #[tracing::instrument(skip_all, err)]
    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        let mut con = self.connection.clone();
        let stored_element_ref: StoredElementReference = element_ref.into();

        let mut pipeline = redis::pipe();
        pipeline.atomic();
        println!("deleting element: {:?}", stored_element_ref);
        self.delete_element_internal(&mut pipeline, &stored_element_ref)
            .await?;

        if let Err(err) = pipeline.query_async::<_, ()>(&mut con).await {
            return Err(IndexError::other(err));
        }

        Ok(())
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let mut con = self.connection.clone();
        let element_key = self.key_formatter.get_element_key(element_ref);

        let stored_slots = match con
            .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "slots")
            .await
        {
            Ok(Some(stored)) => BitSet::from_bytes(stored.as_slice()),
            Ok(None) => return Ok(None),
            Err(e) => return Err(IndexError::other(e)),
        };

        if !stored_slots.contains(slot) {
            return Ok(None);
        }

        let stored: StoredElement = match con
            .hget::<&str, &str, Option<Vec<u8>>>(&element_key, "e")
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

        let element: Element = stored.into();
        Ok(Some(Arc::new(element)))
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let mut con = self.connection.clone();
        let inbound_key = self.key_formatter.get_inbound_key(inbound_ref, slot);
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
                                log::debug!("Garbage collecting reference of deleted element: {}", element_ref);
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

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let mut con = self.connection.clone();
        let outbound_key = self.key_formatter.get_outbound_key(outbound_ref, slot);
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
                                log::debug!("Garbage collecting reference of deleted element: {}", element_ref);
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

    async fn clear(&self) -> Result<(), IndexError> {
        self.connection
            .clear(format!("ei:{}:*", self.query_id))
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
            "ei:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref()
        )
    }

    fn get_stored_element_key(&self, element_ref: &StoredElementReference) -> String {
        format!(
            "ei:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id,
            element_ref.element_id
        )
    }

    fn get_element_key_from_ref_string(&self, element_ref: &str) -> String {
        format!("ei:{}:{}", self.query_id.as_ref(), element_ref)
    }

    fn get_inbound_key(&self, element_ref: &ElementReference, slot: usize) -> String {
        format!(
            "ei:{}:$in:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref(),
            slot
        )
    }

    fn get_outbound_key(&self, element_ref: &ElementReference, slot: usize) -> String {
        format!(
            "ei:{}:$out:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref(),
            slot
        )
    }

    fn get_stored_inbound_key(&self, element_ref: &StoredElementReference, slot: usize) -> String {
        format!(
            "ei:{}:$in:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id,
            element_ref.element_id,
            slot
        )
    }

    fn get_stored_outbound_key(&self, element_ref: &StoredElementReference, slot: usize) -> String {
        format!(
            "ei:{}:$out:{}:{}:{}",
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
            "ei:{}:$partial:{}:{}{}:{}:{}",
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
            "archive:{}:{}:{}",
            self.query_id.as_ref(),
            element_ref.source_id.as_ref(),
            element_ref.element_id.as_ref()
        )
    }

    fn get_stored_archive_key(&self, element_ref: &StoredElementReference) -> String {
        format!(
            "archive:{}:{}:{}",
            self.query_id.as_ref(),
            &element_ref.source_id,
            &element_ref.element_id
        )
    }
}

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
