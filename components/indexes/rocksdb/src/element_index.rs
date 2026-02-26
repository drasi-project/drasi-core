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
    collections::HashMap,
    hash::Hash,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use bit_set::BitSet;
use drasi_core::{
    interface::{ElementIndex, ElementStream, IndexError},
    models::{Element, ElementReference, QueryJoin, QueryJoinKey},
    path_solver::match_path::MatchPath,
};
use hashers::jenkins::spooky_hash::SpookyHasher;
use prost::{bytes::BytesMut, Message};
use rocksdb::{OptimisticTransactionDB, Options, SliceTransform, Transaction};
use tokio::task;

use crate::storage_models::{
    StoredElement, StoredElementContainer, StoredElementMetadata, StoredElementReference,
    StoredRelation, StoredValue, StoredValueMap,
};
use crate::RocksDbSessionState;

mod archive_index;

/// RocksDB element index store
///
/// Column Families
///     - elements [hash128(source_id, element_id)] -> StoredElement
///     - slots    [hash128(source_id, element_id)] -> Slot BitSet
///     - inbound  [hash128(in_source_id, in_element_id) + slot{u16} + hash128(source_id, element_id)] -> null       {18 byte prefix}
///     - outbound [hash128(out_source_id, out_element_id) + slot{u16} + hash128(source_id, element_id)] -> null     {18 byte prefix}
///     - partial  [hash128(join_label, field_value, node_label, property) + len(source_id) + source_id + element_id] -> null {16 byte prefix}
pub struct RocksDbElementIndex {
    context: Arc<Context>,
}

pub struct Context {
    db: Arc<OptimisticTransactionDB>,
    join_spec_by_label: RwLock<JoinSpecByLabel>,
    options: RocksIndexOptions,
    session_state: Arc<RocksDbSessionState>,
}

#[derive(Clone, Copy)]
pub struct RocksIndexOptions {
    pub archive_enabled: bool,
    pub direct_io: bool,
}

const ELEMENTS_CF: &str = "elements";
const SLOT_CF: &str = "slots";
const INBOUND_CF: &str = "inbound";
const OUTBOUND_CF: &str = "outbound";
const PARTIAL_CF: &str = "partial";
const ELEMENT_BLOCK_CACHE_SIZE: u64 = 32;

impl RocksDbElementIndex {
    /// Create a new RocksDbElementIndex from a shared database handle.
    ///
    /// The database must already have the required column families created.
    /// Use `open_unified_db()` to open a database with all required CFs.
    pub fn new(
        db: Arc<OptimisticTransactionDB>,
        options: RocksIndexOptions,
        session_state: Arc<RocksDbSessionState>,
    ) -> Self {
        RocksDbElementIndex {
            context: Arc::new(Context {
                db,
                join_spec_by_label: RwLock::new(HashMap::new()),
                options,
                session_state,
            }),
        }
    }
}

#[async_trait]
impl ElementIndex for RocksDbElementIndex {
    #[tracing::instrument(skip_all, err)]
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let element_key = hash_element_ref(element_ref);
        let context = self.context.clone();
        let task = task::spawn_blocking(move || {
            context.session_state.with_txn(|txn| {
                let stored = get_element_internal(context.clone(), &element_key, txn)?;
                match stored {
                    Some(stored) => {
                        let element: Element = stored.into();
                        Ok(Some(Arc::new(element)))
                    }
                    None => Ok(None),
                }
            })
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let stored: StoredElement = element.into();
        let slot_affinity = slot_affinity.clone();
        let context = self.context.clone();
        let task = task::spawn_blocking(move || {
            context
                .session_state
                .with_txn(|txn| set_element_internal(context.clone(), txn, stored, &slot_affinity))
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        let element_key = hash_element_ref(element_ref);
        let context = self.context.clone();
        let task = task::spawn_blocking(move || {
            context.session_state.with_txn(|txn| {
                delete_element_internal(context.clone(), txn, &element_key)
            })
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let element_key = hash_element_ref(element_ref);
        let context = self.context.clone();
        let task = task::spawn_blocking(move || {
            let slot_cf = context.db.cf_handle(SLOT_CF).expect("Slot CF not found");

            context.session_state.with_txn(|txn| {
                let prev_slots = match txn.get_cf(&slot_cf, element_key) {
                    Ok(Some(prev_slots)) => BitSet::from_bytes(&prev_slots),
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(IndexError::other(e)),
                };

                if !prev_slots.contains(slot) {
                    return Ok(None);
                }

                match get_element_internal(context.clone(), &element_key, txn) {
                    Ok(Some(stored)) => {
                        let element: Element = stored.into();
                        Ok(Some(Arc::new(element)))
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            })
        });

        match task.await {
            Ok(result) => result,
            Err(e) => Err(IndexError::other(e)),
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let context = self.context.clone();
        let element_key = hash_element_ref(inbound_ref);

        let task = task::spawn_blocking(move || {
            let inbound_cf = context
                .db
                .cf_handle(INBOUND_CF)
                .expect("Inbound CF not found");
            let prefix = encode_inout_prefix(&element_key, slot);

            context.session_state.with_txn(|txn| {
                let mut results: Vec<Result<Arc<Element>, IndexError>> = Vec::new();
                for item in txn.prefix_iterator_cf(&inbound_cf, prefix) {
                    match item {
                        Ok((key, _)) => {
                            if !key.starts_with(&prefix) {
                                break;
                            }
                            let relation_key = decode_inout_key_relation(&key);
                            match get_element_internal(context.clone(), &relation_key, txn) {
                                Ok(Some(stored)) => {
                                    let element: Element = stored.into();
                                    results.push(Ok(Arc::new(element)));
                                }
                                Ok(None) => {
                                    log::debug!(
                                        "Garbage collecting reference of deleted element: {element_key:?}"
                                    );
                                    let _ = txn.delete_cf(&inbound_cf, key);
                                    continue;
                                }
                                Err(e) => results.push(Err(e)),
                            };
                        }
                        Err(e) => results.push(Err(IndexError::other(e))),
                    }
                }
                Ok(results)
            })
        });

        let results = match task.await {
            Ok(result) => result?,
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(Box::pin(futures::stream::iter(results)))
    }

    #[tracing::instrument(skip_all, err)]
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let context = self.context.clone();
        let element_key = hash_element_ref(outbound_ref);

        let task = task::spawn_blocking(move || {
            let outbound_cf = context
                .db
                .cf_handle(OUTBOUND_CF)
                .expect("Outbound CF not found");
            let prefix = encode_inout_prefix(&element_key, slot);

            context.session_state.with_txn(|txn| {
                let mut results: Vec<Result<Arc<Element>, IndexError>> = Vec::new();
                for item in txn.prefix_iterator_cf(&outbound_cf, prefix) {
                    match item {
                        Ok((key, _)) => {
                            if !key.starts_with(&prefix) {
                                break;
                            }
                            let relation_key = decode_inout_key_relation(&key);
                            match get_element_internal(context.clone(), &relation_key, txn) {
                                Ok(Some(stored)) => {
                                    let element: Element = stored.into();
                                    results.push(Ok(Arc::new(element)));
                                }
                                Ok(None) => {
                                    log::debug!(
                                        "Garbage collecting reference of deleted element: {element_key:?}"
                                    );
                                    let _ = txn.delete_cf(&outbound_cf, key);
                                    continue;
                                }
                                Err(e) => results.push(Err(e)),
                            };
                        }
                        Err(e) => results.push(Err(IndexError::other(e))),
                    }
                }
                Ok(results)
            })
        });

        let results = match task.await {
            Ok(result) => result?,
            Err(e) => return Err(IndexError::other(e)),
        };

        Ok(Box::pin(futures::stream::iter(results)))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let context = self.context.clone();
        let task = task::spawn_blocking(move || {
            if let Err(err) = context.db.drop_cf(ELEMENTS_CF) {
                return Err(IndexError::other(err));
            }
            if let Err(err) = context.db.drop_cf(SLOT_CF) {
                return Err(IndexError::other(err));
            }
            if let Err(err) = context.db.drop_cf(INBOUND_CF) {
                return Err(IndexError::other(err));
            }
            if let Err(err) = context.db.drop_cf(OUTBOUND_CF) {
                return Err(IndexError::other(err));
            }
            if let Err(err) = context.db.drop_cf(PARTIAL_CF) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = context
                .db
                .create_cf(ELEMENTS_CF, &get_elements_cf_options())
            {
                return Err(IndexError::other(err));
            }

            if let Err(err) = context.db.create_cf(SLOT_CF, &get_elements_cf_options()) {
                return Err(IndexError::other(err));
            }

            if let Err(err) = context
                .db
                .create_cf(INBOUND_CF, &get_inout_index_cf_options())
            {
                return Err(IndexError::other(err));
            }

            if let Err(err) = context
                .db
                .create_cf(OUTBOUND_CF, &get_inout_index_cf_options())
            {
                return Err(IndexError::other(err));
            }

            if let Err(err) = context.db.create_cf(PARTIAL_CF, &get_partial_cf_options()) {
                return Err(IndexError::other(err));
            }

            Ok(())
        });

        match task.await {
            Ok(v) => v,
            Err(err) => Err(IndexError::other(err)),
        }
    }

    #[allow(clippy::unwrap_used)]
    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        let joins_by_label = extract_join_spec_by_label(match_path, joins);
        let mut join_spec_by_label = self.context.join_spec_by_label.write().unwrap();
        join_spec_by_label.clone_from(&joins_by_label);
    }
}

pub(crate) fn get_partial_cf_options() -> Options {
    let mut partial_opts = Options::default();
    partial_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(16));
    partial_opts
}

pub(crate) fn get_inout_index_cf_options() -> Options {
    let mut inout_bound_opts = Options::default();
    inout_bound_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(18));
    inout_bound_opts
}

pub(crate) fn get_elements_cf_options() -> Options {
    let mut elements_opts = Options::default();
    elements_opts.optimize_for_point_lookup(ELEMENT_BLOCK_CACHE_SIZE);
    elements_opts
}

/// Collect all column family descriptors needed by the element index.
pub(crate) fn element_cf_descriptors(
    options: &RocksIndexOptions,
) -> Vec<rocksdb::ColumnFamilyDescriptor> {
    let mut cfs = vec![
        rocksdb::ColumnFamilyDescriptor::new(ELEMENTS_CF, get_elements_cf_options()),
        rocksdb::ColumnFamilyDescriptor::new(SLOT_CF, get_elements_cf_options()),
        rocksdb::ColumnFamilyDescriptor::new(INBOUND_CF, get_inout_index_cf_options()),
        rocksdb::ColumnFamilyDescriptor::new(OUTBOUND_CF, get_inout_index_cf_options()),
        rocksdb::ColumnFamilyDescriptor::new(PARTIAL_CF, get_partial_cf_options()),
    ];

    if options.archive_enabled {
        cfs.push(rocksdb::ColumnFamilyDescriptor::new(
            archive_index::ARCHIVE_CF,
            archive_index::get_archive_cf_options(),
        ));
    }

    cfs
}

fn get_element_internal(
    context: Arc<Context>,
    element_key: &ReferenceHash,
    txn: &Transaction<'_, OptimisticTransactionDB>,
) -> Result<Option<StoredElement>, IndexError> {
    let element_cf = context
        .db
        .cf_handle(ELEMENTS_CF)
        .expect("Element CF not found");

    let element = match txn.get_cf(&element_cf, element_key) {
        Ok(Some(element)) => element,
        Ok(None) => return Ok(None),
        Err(e) => return Err(IndexError::other(e)),
    };

    let element = match StoredElementContainer::decode(element.as_slice()) {
        Ok(container) => container.element,
        Err(e) => return Err(IndexError::other(e)),
    };

    Ok(element)
}

fn delete_element_internal(
    context: Arc<Context>,
    txn: &Transaction<'_, OptimisticTransactionDB>,
    element_key: &ReferenceHash,
) -> Result<(), IndexError> {
    let element_cf = context
        .db
        .cf_handle(ELEMENTS_CF)
        .expect("Element CF not found");
    let slot_cf = context.db.cf_handle(SLOT_CF).expect("Slot CF not found");
    let inbound_cf = context
        .db
        .cf_handle(INBOUND_CF)
        .expect("Inbound CF not found");
    let outbound_cf = context
        .db
        .cf_handle(OUTBOUND_CF)
        .expect("Outbound CF not found");

    let prev_slots = match txn.get_cf(&slot_cf, element_key) {
        Ok(Some(prev_slots)) => Some(BitSet::from_bytes(&prev_slots)),
        Ok(None) => None,
        Err(e) => return Err(IndexError::other(e)),
    };

    match get_element_internal(context.clone(), element_key, txn) {
        Ok(Some(old_element)) => delete_source_joins(context.clone(), txn, &old_element)?,
        Ok(None) => (),
        Err(e) => return Err(IndexError::other(e)),
    }

    match txn.delete_cf(&element_cf, element_key) {
        Ok(_) => log::debug!("Deleted element {element_key:?}"),
        Err(e) => return Err(IndexError::other(e)),
    };

    match txn.delete_cf(&slot_cf, element_key) {
        Ok(_) => log::debug!("Deleted element slots {element_key:?}"),
        Err(e) => return Err(IndexError::other(e)),
    };

    if let Some(prev_slots) = prev_slots {
        for slot in prev_slots.into_iter() {
            let prefix = encode_inout_prefix(element_key, slot);

            for item in txn.prefix_iterator_cf(&inbound_cf, prefix) {
                match item {
                    Ok((key, _)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        match txn.delete_cf(&inbound_cf, key) {
                            Ok(_) => {
                                log::debug!("Deleted inbound slot index {element_key:?} {slot:?}")
                            }
                            Err(e) => return Err(IndexError::other(e)),
                        };
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }

            for item in txn.prefix_iterator_cf(&outbound_cf, prefix) {
                match item {
                    Ok((key, _)) => {
                        if !key.starts_with(&prefix) {
                            break;
                        }
                        match txn.delete_cf(&outbound_cf, key) {
                            Ok(_) => {
                                log::debug!("Deleted outbound slot index {element_key:?} {slot:?}")
                            }
                            Err(e) => return Err(IndexError::other(e)),
                        };
                    }
                    Err(e) => return Err(IndexError::other(e)),
                }
            }
        }
    }

    Ok(())
}

fn set_element_internal(
    context: Arc<Context>,
    txn: &Transaction<OptimisticTransactionDB>,
    element: StoredElement,
    slot_affinity: &Vec<usize>,
) -> Result<(), IndexError> {
    let eref = element.get_reference();
    let key_hash = hash_stored_element_ref(eref);
    let element_cf = context
        .db
        .cf_handle(ELEMENTS_CF)
        .expect("Element CF not found");
    let slot_cf = context.db.cf_handle(SLOT_CF).expect("Slot CF not found");
    let inbound_cf = context
        .db
        .cf_handle(INBOUND_CF)
        .expect("Inbound CF not found");
    let outbound_cf = context
        .db
        .cf_handle(OUTBOUND_CF)
        .expect("Outbound CF not found");

    let prev_slots = match txn.get_cf(&slot_cf, key_hash) {
        Ok(Some(prev_slots)) => Some(BitSet::from_bytes(&prev_slots)),
        Ok(None) => None,
        Err(e) => return Err(IndexError::other(e)),
    };

    let new_slots = slots_to_bitset(slot_affinity);

    let relation_nodes = match &element {
        StoredElement::Relation(r) => Some((r.in_node.clone(), r.out_node.clone())),
        _ => None,
    };

    let (element, encoded_element) = {
        let mut buf = BytesMut::new();
        let container = StoredElementContainer {
            element: Some(element),
        };
        if let Err(e) = container.encode(&mut buf) {
            return Err(IndexError::other(e));
        }
        (
            match container.element {
                Some(e) => e,
                None => return Err(IndexError::CorruptedData),
            },
            buf.freeze(),
        )
    };

    match txn.put_cf(&element_cf, key_hash, &encoded_element) {
        Ok(_) => log::debug!("Stored element {key_hash:?}"),
        Err(e) => return Err(IndexError::other(e)),
    };

    match txn.put_cf(
        &slot_cf,
        key_hash,
        new_slots.clone().into_bit_vec().to_bytes(),
    ) {
        Ok(_) => log::debug!("Stored element slots {key_hash:?}"),
        Err(e) => return Err(IndexError::other(e)),
    };

    if let Some((in_node, out_node)) = relation_nodes {
        let mut slots_changed = true;

        if let Some(prev_slots) = prev_slots {
            if prev_slots == new_slots {
                slots_changed = false;
            }

            if slots_changed {
                for slot in prev_slots.into_iter() {
                    let inbound_key = encode_inout_key(&in_node, slot, &key_hash);
                    let outbound_key = encode_inout_key(&out_node, slot, &key_hash);

                    if let Err(err) = txn.delete_cf(&inbound_cf, inbound_key) {
                        log::error!(
                            "Failed to delete inbound index {inbound_key:?} for element {key_hash:?}: {err:?}"
                        );
                        return Err(IndexError::other(err));
                    }

                    if let Err(err) = txn.delete_cf(&outbound_cf, outbound_key) {
                        log::error!(
                            "Failed to delete outbound index {outbound_key:?} for element {key_hash:?}: {err:?}"
                        );
                        return Err(IndexError::other(err));
                    }
                }
            }
        }

        if slots_changed {
            for slot in slot_affinity {
                let inbound_key = encode_inout_key(&in_node, *slot, &key_hash);
                let outbound_key = encode_inout_key(&out_node, *slot, &key_hash);

                if let Err(err) = txn.put_cf(&inbound_cf, inbound_key, []) {
                    log::error!(
                        "Failed to store inbound index {inbound_key:?} for element {key_hash:?}: {err:?}"
                    );
                    return Err(IndexError::other(err));
                }

                if let Err(err) = txn.put_cf(&outbound_cf, outbound_key, []) {
                    log::error!(
                        "Failed to store outbound index {outbound_key:?} for element {key_hash:?}: {err:?}"
                    );
                    return Err(IndexError::other(err));
                }
            }
        }
    }

    update_source_joins(context.clone(), txn, &element)?;

    if context.options.archive_enabled {
        archive_index::insert_archive(
            context.clone(),
            txn,
            key_hash,
            &encoded_element,
            element.get_effective_from(),
        )?;
    }

    Ok(())
}

fn update_source_joins(
    context: Arc<Context>,
    txn: &Transaction<OptimisticTransactionDB>,
    new_element: &StoredElement,
) -> Result<(), IndexError> {
    match new_element {
        StoredElement::Node(n) => {
            let join_spec_by_label = match context.join_spec_by_label.read() {
                Ok(joins) => joins,
                Err(_) => return Err(IndexError::CorruptedData),
            };
            for (label, joins) in join_spec_by_label.iter() {
                if !n.metadata.labels.contains(label) {
                    continue;
                }

                let partial_cf = context
                    .db
                    .cf_handle(PARTIAL_CF)
                    .expect("Partial CF not found");

                for (qj, slots) in joins {
                    for qjk in &qj.keys {
                        if qjk.label != *label {
                            continue;
                        }

                        match n.properties.get(&qjk.property) {
                            Some(new_value) => {
                                let element_key = hash_stored_element_ref(&n.metadata.reference);
                                // Read from committed DB to get the OLD element state.
                                // The new element was already written to the txn, so reading
                                // through the txn would return the new value and the property
                                // comparison below would always see "no change".
                                let element_cf = context
                                    .db
                                    .cf_handle(ELEMENTS_CF)
                                    .expect("Element CF not found");
                                let old_element = match context.db.get_cf(&element_cf, element_key)
                                {
                                    Ok(Some(data)) => {
                                        match StoredElementContainer::decode(data.as_slice()) {
                                            Ok(container) => container.element,
                                            Err(e) => return Err(IndexError::other(e)),
                                        }
                                    }
                                    Ok(None) => None,
                                    Err(e) => return Err(IndexError::other(e)),
                                };

                                if let Some(StoredElement::Node(old)) = &old_element {
                                    if let Some(old_value) = old.properties.get(&qjk.property) {
                                        if old_value == new_value {
                                            continue;
                                        }
                                    }
                                }

                                let pj_prefix = encode_partial_join_prefix(
                                    &qj.id,
                                    new_value,
                                    &qjk.label,
                                    &qjk.property,
                                );
                                let pj_key =
                                    encode_partial_join_key(&pj_prefix, &n.metadata.reference);

                                let did_insert = match txn.get_cf(&partial_cf, &pj_key) {
                                    Ok(Some(_)) => false,
                                    Ok(None) => {
                                        if let Err(err) = txn.put_cf(&partial_cf, &pj_key, []) {
                                            log::error!("Failed to store partial join {:?} for element {:?}: {:?}", pj_key, &n.metadata.reference, err);
                                            return Err(IndexError::other(err));
                                        }
                                        true
                                    }
                                    Err(e) => return Err(IndexError::other(e)),
                                };

                                if did_insert {
                                    //remove old partial joins
                                    if let Some(old_element) = &old_element {
                                        if let StoredElement::Node(old) = old_element {
                                            if let Some(old_value) =
                                                old.properties.get(&qjk.property)
                                            {
                                                delete_source_join(
                                                    context.clone(),
                                                    txn,
                                                    old_element.get_reference(),
                                                    qj,
                                                    qjk,
                                                    old_value,
                                                )?;
                                            }
                                        }
                                    }

                                    let element_reference = n.metadata.reference.clone();

                                    //find matching counterparts
                                    for qjk2 in &qj.keys {
                                        if qjk == qjk2 {
                                            continue;
                                        }

                                        let other_pj_prefix = encode_partial_join_prefix(
                                            &qj.id,
                                            new_value,
                                            &qjk2.label,
                                            &qjk2.property,
                                        );
                                        let others =
                                            txn.prefix_iterator_cf(&partial_cf, other_pj_prefix);

                                        for other in others {
                                            let other = match other {
                                                Ok((k, _)) => {
                                                    if !k.starts_with(&other_pj_prefix) {
                                                        break;
                                                    }
                                                    decode_partial_join_key(&k)?
                                                }
                                                Err(e) => return Err(IndexError::other(e)),
                                            };

                                            let in_out = StoredElement::Relation(StoredRelation {
                                                metadata: StoredElementMetadata {
                                                    reference: get_join_virtual_ref(
                                                        &element_reference,
                                                        &other,
                                                    ),
                                                    labels: vec![qj.id.clone()],
                                                    effective_from: n.metadata.effective_from,
                                                },
                                                in_node: element_reference.clone(),
                                                out_node: other.clone(),
                                                properties: StoredValueMap::new(),
                                            });

                                            let out_in = StoredElement::Relation(StoredRelation {
                                                metadata: StoredElementMetadata {
                                                    reference: get_join_virtual_ref(
                                                        &other,
                                                        &element_reference,
                                                    ),
                                                    labels: vec![qj.id.clone()],
                                                    effective_from: n.metadata.effective_from,
                                                },
                                                in_node: other.clone(),
                                                out_node: element_reference.clone(),
                                                properties: StoredValueMap::new(),
                                            });

                                            set_element_internal(
                                                context.clone(),
                                                txn,
                                                in_out,
                                                slots,
                                            )?;
                                            set_element_internal(
                                                context.clone(),
                                                txn,
                                                out_in,
                                                slots,
                                            )?;
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

fn delete_source_joins(
    context: Arc<Context>,
    txn: &Transaction<OptimisticTransactionDB>,
    old_element: &StoredElement,
) -> Result<(), IndexError> {
    match old_element {
        StoredElement::Node(n) => {
            let join_spec_by_label = match context.join_spec_by_label.read() {
                Ok(joins) => joins,
                Err(_) => return Err(IndexError::CorruptedData),
            };
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
                                delete_source_join(
                                    context.clone(),
                                    txn,
                                    old_element.get_reference(),
                                    qj,
                                    qjk,
                                    value,
                                )?;
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

fn delete_source_join(
    context: Arc<Context>,
    txn: &Transaction<OptimisticTransactionDB>,
    old_element: &StoredElementReference,
    query_join: &QueryJoin,
    join_key: &QueryJoinKey,
    value: &StoredValue,
) -> Result<(), IndexError> {
    let partial_cf = context
        .db
        .cf_handle(PARTIAL_CF)
        .expect("Partial CF not found");

    let pj_prefix =
        encode_partial_join_prefix(&query_join.id, value, &join_key.label, &join_key.property);
    let pj_key = encode_partial_join_key(&pj_prefix, old_element);

    let did_remove = match txn.get_cf(&partial_cf, &pj_key) {
        Ok(Some(_)) => false,
        Ok(None) => {
            if let Err(err) = txn.delete_cf(&partial_cf, &pj_key) {
                return Err(IndexError::other(err));
            }
            true
        }
        Err(e) => return Err(IndexError::other(e)),
    };

    if did_remove {
        for qjk2 in &query_join.keys {
            if join_key == qjk2 {
                continue;
            }

            let other_pj_prefix =
                encode_partial_join_prefix(&query_join.id, value, &qjk2.label, &qjk2.property);
            let others = txn.prefix_iterator_cf(&partial_cf, other_pj_prefix);

            for other in others {
                let other = match other {
                    Ok((k, _)) => {
                        if !k.starts_with(&other_pj_prefix) {
                            break;
                        }
                        decode_partial_join_key(&k)?
                    }
                    Err(e) => return Err(IndexError::other(e)),
                };

                let in_out = get_join_virtual_ref(old_element, &other);
                let out_in = get_join_virtual_ref(&other, old_element);

                delete_element_internal(
                    context.clone(),
                    txn,
                    &hash_stored_element_ref(&in_out),
                )?;
                delete_element_internal(
                    context.clone(),
                    txn,
                    &hash_stored_element_ref(&out_in),
                )?;
            }
        }
    }

    Ok(())
}

pub type ReferenceHash = [u8; 16];

fn hash_element_ref(element_ref: &ElementReference) -> ReferenceHash {
    let bytes = element_ref
        .source_id
        .as_bytes()
        .iter()
        .chain(element_ref.element_id.as_bytes())
        .copied()
        .collect::<Vec<u8>>();

    fastmurmur3::hash(bytes.as_slice()).to_be_bytes()
}

fn hash_stored_element_ref(element_ref: &StoredElementReference) -> ReferenceHash {
    let bytes = element_ref
        .source_id
        .as_bytes()
        .iter()
        .chain(element_ref.element_id.as_bytes())
        .copied()
        .collect::<Vec<u8>>();

    fastmurmur3::hash(bytes.as_slice()).to_be_bytes()
}

fn encode_inout_key(
    element: &StoredElementReference,
    slot: usize,
    relation_key: &ReferenceHash,
) -> [u8; 34] {
    let element = hash_stored_element_ref(element);
    let slot = slot as u16;
    let mut key = [0u8; 34];
    key[0..16].copy_from_slice(&element);
    key[16..18].copy_from_slice(&slot.to_be_bytes());
    key[18..34].copy_from_slice(relation_key);
    key
}

fn encode_inout_prefix(element: &ReferenceHash, slot: usize) -> [u8; 18] {
    let slot = slot as u16;
    let mut key = [0u8; 18];
    key[0..16].copy_from_slice(element);
    key[16..18].copy_from_slice(&slot.to_be_bytes());
    key
}

fn decode_inout_key_relation(key: &[u8]) -> ReferenceHash {
    let mut relation_key = [0u8; 16];
    relation_key.copy_from_slice(&key[18..34]);
    relation_key
}

fn slots_to_bitset(slots: &Vec<usize>) -> BitSet {
    let mut bs = BitSet::new();
    for slot in slots {
        bs.insert(*slot);
    }
    bs
}

type JoinSpecByLabel = HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>>;

#[allow(clippy::type_complexity)]
fn extract_join_spec_by_label(
    match_path: &MatchPath,
    joins: &Vec<Arc<QueryJoin>>,
) -> JoinSpecByLabel {
    let mut result: HashMap<String, Vec<(Arc<QueryJoin>, Vec<usize>)>> = HashMap::new();

    for join in joins {
        let mut slots = Vec::new();
        for (slot_num, slot) in match_path.slots.iter().enumerate() {
            if slot.spec.labels.contains(&Arc::from(join.id.clone())) {
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

///hash128(join_label, field_value, node_label, property)
fn encode_partial_join_prefix(
    join_label: &str,
    field_value: &StoredValue,
    label: &str,
    property: &str,
) -> [u8; 16] {
    let mut hash = SpookyHasher::default();
    join_label.hash(&mut hash);
    field_value.hash(&mut hash);
    label.hash(&mut hash);
    property.hash(&mut hash);

    let result = hash.finish128();
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&result.0.to_be_bytes());
    key[8..16].copy_from_slice(&result.1.to_be_bytes());
    key
}

fn encode_partial_join_key(prefix: &[u8; 16], element_ref: &StoredElementReference) -> Vec<u8> {
    let source_id = element_ref.source_id.as_bytes();
    let source_id_size = source_id.len() as u8;
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(prefix);
    data.push(source_id_size);
    data.extend_from_slice(source_id);
    data.extend_from_slice(element_ref.element_id.as_bytes());
    data
}

fn decode_partial_join_key(key: &[u8]) -> Result<StoredElementReference, IndexError> {
    if key.len() < 17 {
        return Err(IndexError::CorruptedData);
    }
    let source_id_size = key[16];
    let source_id = match String::from_utf8(key[17..(17 + source_id_size as usize)].to_vec()) {
        Ok(s) => s,
        Err(_) => return Err(IndexError::CorruptedData),
    };
    let element_id = match String::from_utf8(key[(17 + source_id_size as usize)..].to_vec()) {
        Ok(s) => s,
        Err(_) => return Err(IndexError::CorruptedData),
    };
    Ok(StoredElementReference {
        source_id,
        element_id,
    })
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
