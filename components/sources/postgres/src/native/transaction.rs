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
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::computation::v1::SourceTransactionBuilder;

pub(super) type TableKeys = BTreeMap<(String, String), Vec<String>>;

pub(super) struct Transactions {
    pub decoder: crate::decoder::PgOutputDecoder,
    pub pending: Option<(TransactionInfo, SourceTransactionBuilder)>,
    pub keys: TableKeys,
    pub source: ComponentId,
    pub stream: StreamId,
    pub limits: SourceTransactionLimits,
    pub binding: [u8; 32],
    pub resume: u64,
    pub transport_sequence: Arc<AtomicU64>,
    pub filtered: bool,
}

impl Transactions {
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<OutputEnvelope>> {
        let Some(message) = self.decoder.decode_message(data)? else {
            return Ok(None);
        };
        match message {
            WalMessage::Begin(info) => {
                anyhow::ensure!(self.pending.is_none(), "incomplete PostgreSQL transaction");
                self.pending = Some((
                    info,
                    SourceTransactionBuilder::new(self.source.as_str(), self.limits)?,
                ));
            }
            WalMessage::Commit(info) => {
                let (_, builder) = self.pending.take().context("Commit without assembly")?;
                if info.commit_lsn <= self.resume {
                    return Ok(None);
                }
                let position = Position {
                    version: 1,
                    binding: self.binding,
                    commit_lsn: info.commit_lsn,
                };
                let transaction = builder.commit(
                    Bytes::copy_from_slice(&info.commit_lsn.to_be_bytes()),
                    Bytes::from(serde_json::to_vec(&position)?),
                )?;
                let transport = self
                    .transport_sequence
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                        value.checked_add(1)
                    })
                    .map_err(|_| anyhow::anyhow!("PostgreSQL transport sequence exhausted"))?
                    + 1;
                let envelope = transaction.into_replay_envelope(
                    self.stream.clone(),
                    info.commit_lsn,
                    transport,
                    info.commit_timestamp,
                )?;
                self.resume = info.commit_lsn;
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope,
                }));
            }
            WalMessage::Relation(relation) => {
                if self
                    .keys
                    .contains_key(&(relation.namespace.clone(), relation.name.clone()))
                {
                    self.validate_keys(&relation)?;
                } else {
                    anyhow::ensure!(
                        self.filtered,
                        "PostgreSQL publication membership changed; reconstruct the source"
                    );
                }
            }
            WalMessage::Insert { relation_id, tuple } => {
                if let Some(element) = self.element(relation_id, &tuple)? {
                    anyhow::ensure!(
                        !tuple.iter().any(PostgresValue::is_unchanged_toast),
                        "INSERT contains unchanged TOAST values"
                    );
                    self.push(SourceChange::Insert { element })?;
                }
            }
            WalMessage::Update {
                relation_id,
                old_tuple,
                mut new_tuple,
            } => {
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
                        self.push(SourceChange::Delete {
                            metadata: before.get_metadata().clone(),
                        })?;
                        self.push(SourceChange::Insert { element })?;
                        return Ok(None);
                    }
                }
                self.push(SourceChange::Update { element })?;
            }
            WalMessage::Delete {
                relation_id,
                old_tuple,
            } => {
                if let Some(element) = self.element(relation_id, &old_tuple)? {
                    self.push(SourceChange::Delete {
                        metadata: element.get_metadata().clone(),
                    })?;
                }
            }
            WalMessage::Truncate { .. } => anyhow::bail!("transactional TRUNCATE is unsupported"),
        }
        Ok(None)
    }

    fn push(&mut self, change: SourceChange) -> Result<()> {
        let (info, builder) = self.pending.as_mut().context("row outside transaction")?;
        if info.commit_lsn > self.resume {
            builder.push(change)?;
        }
        Ok(())
    }

    fn validate_keys(&self, relation: &RelationInfo) -> Result<()> {
        let keys = &self.keys[&(relation.namespace.clone(), relation.name.clone())];
        anyhow::ensure!(
            !keys.is_empty(),
            "table {} has no stable key",
            relation.name
        );
        for key in keys {
            let column = relation
                .columns
                .iter()
                .find(|column| &column.name == key)
                .with_context(|| format!("key column {key} missing from {}", relation.name))?;
            anyhow::ensure!(
                relation.replica_identity == ReplicaIdentity::Full || column.is_key,
                "replica identity does not carry key column {key}"
            );
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
        let (info, _) = self.pending.as_ref().context("row outside transaction")?;
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

pub(super) fn row_element(
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
    let identity =
        drasi_postgres_common::transaction_element_id(&relation.namespace, &relation.name, &parts)?;
    let label = if relation.namespace == "public" {
        relation.name.clone()
    } else {
        format!("{}.{}", relation.namespace, relation.name)
    };
    Ok(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source.as_str(), &identity),
            labels: Arc::from([Arc::from(label)]),
            effective_from: timestamp,
        },
        properties,
    })
}
