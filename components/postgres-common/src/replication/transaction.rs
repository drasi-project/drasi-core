// Copyright 2026 The Drasi Authors.
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

use super::*;
use crate::types::{PostgresValue, RelationInfo, ReplicaIdentity, TransactionInfo, WalMessage};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::sync::Arc;

/// Decodes pgoutput into the changes of transactions this source has not
/// handled yet. It maps rows to elements and checks key bindings, but
/// assembles nothing: whole-transaction and row outputs each consume the
/// events their own way, so no transaction is encoded twice.
pub struct Transactions {
    pub decoder: crate::decoder::PgOutputDecoder,
    pub keys: TableKeys,
    pub source: ComponentId,
    pub binding: [u8; 32],
    pub managed_slot: Option<String>,
    /// Everything at or below this LSN is handled: commits at or below it
    /// are skipped.
    pub resume: u64,
    pub filtered: bool,
    open: Option<Open>,
}

struct Open {
    info: TransactionInfo,
    /// The transaction is already handled and is skipped.
    skip: bool,
}

/// One decoded event of a transaction the source has not handled.
pub enum Decoded {
    /// A transaction opened; its commit LSN and timestamp are already known.
    Begin(TransactionInfo),
    /// Changes of the open transaction, in order.
    Changes(Changes),
    /// The open transaction committed. `resume` has advanced to it.
    Commit(TransactionInfo),
}

/// The changes of one pgoutput message: one change, or a key-changing
/// update's delete and insert.
pub struct Changes {
    first: SourceChange,
    second: Option<SourceChange>,
    /// The pgoutput message size, an estimate of the changes' size.
    pub bytes: usize,
}

impl Changes {
    pub fn len(&self) -> usize {
        1 + usize::from(self.second.is_some())
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

impl IntoIterator for Changes {
    type Item = SourceChange;
    type IntoIter =
        std::iter::Chain<std::iter::Once<SourceChange>, std::option::IntoIter<SourceChange>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.first).chain(self.second)
    }
}

impl Transactions {
    pub fn new(
        decoder: crate::decoder::PgOutputDecoder,
        keys: TableKeys,
        source: ComponentId,
        binding: [u8; 32],
        managed_slot: Option<String>,
        resume: u64,
        filtered: bool,
    ) -> Self {
        Self {
            decoder,
            keys,
            source,
            binding,
            managed_slot,
            resume,
            filtered,
            open: None,
        }
    }

    /// Whether a transaction is open: between Begin and Commit.
    pub fn in_transaction(&self) -> bool {
        self.open.is_some()
    }

    /// Restarts decoding on a new replication connection.
    pub fn restart(
        &mut self,
        decoder: crate::decoder::PgOutputDecoder,
        keys: TableKeys,
        resume: u64,
    ) {
        self.decoder = decoder;
        self.keys = keys;
        self.resume = resume;
        self.open = None;
    }

    /// Records that everything up to `lsn` is handled: a boundary proven to
    /// hold no unhandled commit, such as a keepalive's WAL end.
    pub fn advance(&mut self, lsn: u64) {
        debug_assert!(self.open.is_none(), "advanced inside a transaction");
        self.resume = self.resume.max(lsn);
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Decoded>> {
        let Some(message) = self.decoder.decode_message(data)? else {
            return Ok(None);
        };
        let bytes = data.len();
        match message {
            WalMessage::Begin(info) => {
                anyhow::ensure!(self.open.is_none(), "incomplete PostgreSQL transaction");
                let skip = info.commit_lsn <= self.resume;
                self.open = Some(Open {
                    info: info.clone(),
                    skip,
                });
                if !skip {
                    return Ok(Some(Decoded::Begin(info)));
                }
            }
            WalMessage::Commit(info) => {
                let open = self.open.take().context("Commit without assembly")?;
                if !open.skip {
                    self.resume = info.commit_lsn;
                    return Ok(Some(Decoded::Commit(info)));
                }
            }
            WalMessage::Relation(relation) => {
                if self
                    .keys
                    .contains_key(&(relation.namespace.clone(), relation.name.clone()))
                {
                    self.validate_keys(&relation)?;
                } else if !self.filtered {
                    return Err(PostgresCdcError::SchemaChanged(format!(
                        "table {}.{} joined the publication",
                        relation.namespace, relation.name
                    ))
                    .into());
                }
            }
            WalMessage::Insert { relation_id, tuple } => {
                if self.skipping()? {
                    return Ok(None);
                }
                if let Some(element) = self.element(relation_id, &tuple)? {
                    anyhow::ensure!(
                        !tuple.iter().any(PostgresValue::is_unchanged_toast),
                        "INSERT contains unchanged TOAST values"
                    );
                    return Ok(Some(changes(SourceChange::Insert { element }, None, bytes)));
                }
            }
            WalMessage::Update {
                relation_id,
                old_tuple,
                mut new_tuple,
            } => {
                if self.skipping()? {
                    return Ok(None);
                }
                let Some(mut element) = self.element(relation_id, &new_tuple)? else {
                    return Ok(None);
                };
                if let Some(old) = old_tuple {
                    let before = self
                        .element(relation_id, &old)?
                        .context("old row mapping")?;
                    if before.get_reference() != element.get_reference() {
                        let relation =
                            self.decoder.get_relation(relation_id).context("relation")?;
                        for (index, value) in new_tuple.iter_mut().enumerate() {
                            if value.is_unchanged_toast() {
                                anyhow::ensure!(
                                    relation.replica_identity == ReplicaIdentity::Full
                                        && !old[index].is_unchanged_toast(),
                                    "key-changing UPDATE requires complete before/after values"
                                );
                                *value = old[index].clone();
                            }
                        }
                        element = self
                            .element(relation_id, &new_tuple)?
                            .context("new row mapping")?;
                        return Ok(Some(changes(
                            SourceChange::Delete {
                                metadata: before.get_metadata().clone(),
                            },
                            Some(SourceChange::Insert { element }),
                            bytes,
                        )));
                    }
                }
                return Ok(Some(changes(SourceChange::update(element), None, bytes)));
            }
            WalMessage::Delete {
                relation_id,
                old_tuple,
            } => {
                if self.skipping()? {
                    return Ok(None);
                }
                if let Some(element) = self.element(relation_id, &old_tuple)? {
                    return Ok(Some(changes(
                        SourceChange::Delete {
                            metadata: element.get_metadata().clone(),
                        },
                        None,
                        bytes,
                    )));
                }
            }
            WalMessage::Truncate { relation_ids } => {
                if self.skipping()? {
                    return Ok(None);
                }
                // Decision D-9: a TRUNCATE removes rows without row changes,
                // and no source reset can be expressed, so a TRUNCATE of a
                // selected table is a typed permanent failure. Truncating
                // only unselected tables changes nothing this source emits.
                for relation_id in relation_ids {
                    let relation = self
                        .decoder
                        .get_relation(relation_id)
                        .context("TRUNCATE of an unknown relation")?;
                    if self
                        .keys
                        .contains_key(&(relation.namespace.clone(), relation.name.clone()))
                    {
                        return Err(PostgresCdcError::Unsupported(format!(
                            "TRUNCATE of selected table {}.{} cannot be represented as row changes; \
                             publish only insert, update and delete, then remove the source with \
                             erase and add it again to rebuild from a new snapshot",
                            relation.namespace, relation.name
                        ))
                        .into());
                    }
                }
            }
        }
        Ok(None)
    }

    /// Whether the open transaction is already handled.
    fn skipping(&self) -> Result<bool> {
        Ok(self.open.as_ref().context("row outside transaction")?.skip)
    }

    fn validate_keys(&self, relation: &RelationInfo) -> Result<()> {
        let keys = &self.keys[&(relation.namespace.clone(), relation.name.clone())];
        anyhow::ensure!(
            !keys.is_empty(),
            "table {} has no stable key",
            relation.name
        );
        for key in keys {
            let carried = relation.columns.iter().find(|column| &column.name == key);
            match carried {
                None => {
                    return Err(PostgresCdcError::SchemaChanged(format!(
                        "key column {key} is missing from {}.{}",
                        relation.namespace, relation.name
                    ))
                    .into())
                }
                Some(column)
                    if relation.replica_identity != ReplicaIdentity::Full && !column.is_key =>
                {
                    return Err(PostgresCdcError::SchemaChanged(format!(
                        "the replica identity of {}.{} does not carry key column {key}",
                        relation.namespace, relation.name
                    ))
                    .into())
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn element(&self, id: u32, tuple: &[PostgresValue]) -> Result<Option<Element>> {
        let relation = self.decoder.get_relation(id).context("unknown relation")?;
        let Some(keys) = self
            .keys
            .get(&(relation.namespace.clone(), relation.name.clone()))
        else {
            return Ok(None);
        };
        self.validate_keys(relation)?;
        let info = &self.open.as_ref().context("row outside transaction")?.info;
        row_element(
            &self.source,
            relation,
            keys,
            tuple,
            info.commit_timestamp.timestamp_millis().try_into()?,
        )
        .map(Some)
    }
}

fn changes(first: SourceChange, second: Option<SourceChange>, bytes: usize) -> Decoded {
    Decoded::Changes(Changes {
        first,
        second,
        bytes,
    })
}

pub fn row_element(
    source: &ComponentId,
    relation: &RelationInfo,
    keys: &[String],
    tuple: &[PostgresValue],
    timestamp: u64,
) -> Result<Element> {
    anyhow::ensure!(
        tuple.len() == relation.columns.len(),
        "incomplete PostgreSQL row"
    );
    let mut parts = Vec::with_capacity(keys.len());
    let mut properties = ElementPropertyMap::new();
    for (column, value) in relation.columns.iter().zip(tuple) {
        if keys.contains(&column.name) {
            parts.push((
                column.name.clone(),
                value.to_key_string().with_context(|| {
                    format!("missing stable key {}.{}", relation.name, column.name)
                })?,
            ));
        }
        if !value.is_unchanged_toast() {
            properties.insert(&column.name, value.to_element_value());
        }
    }
    anyhow::ensure!(parts.len() == keys.len(), "incomplete PostgreSQL key");
    let identity = crate::transaction_element_id(&relation.namespace, &relation.name, &parts)?;
    let label = super::schema::table_label(&relation.namespace, &relation.name);
    Ok(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source.as_str(), &identity),
            labels: Arc::from([Arc::from(label)]),
            effective_from: timestamp,
        },
        properties,
    })
}
