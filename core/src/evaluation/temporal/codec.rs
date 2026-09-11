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

//! One binary format for both persistent providers, including queue ticket payloads.

use std::collections::BTreeMap;

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use crate::evaluation::variable_value::VariableValue;

use super::{
    value::{StoredGroupingValue, StoredValue},
    FunctionCellId, FutureTicket, OriginKey, PartId, QueryEpoch, TemporalBatch, TemporalCatalog,
    TemporalGroupId, TemporalInputId, TemporalKey, TemporalMutation, TemporalNamespace,
    TemporalRecord, TemporalStateError,
};

pub const CODEC_VERSION: u16 = 1;
const MAGIC: &[u8; 4] = b"DTMP";
pub const KEY_PREFIX: &[u8] = b"drasi:temporal:";
pub const CATALOG_KEY: &[u8] = b"drasi:temporal:catalog";
const VALUE_KIND: u8 = 32;
const TICKET_KIND: u8 = 33;
const KEY_KIND: u8 = 34;
const CATALOG_KIND: u8 = 35;

#[derive(Debug, Error)]
pub enum TemporalCodecError {
    #[error("legacy temporal state cannot reconstruct retained inputs; explicit history replay or snapshot-reset rebuild is required")]
    MigrationRequired,
    #[error("unsupported temporal codec version {found}; supported version is {CODEC_VERSION}")]
    UnsupportedVersion { found: u16 },
    #[error("unsupported temporal record kind {kind}")]
    UnsupportedRecord { kind: u8 },
    #[error(
        "temporal plan format {found} does not match {expected}; rebuild into a new query epoch"
    )]
    PlanVersionMismatch { expected: u32, found: u32 },
    #[error("corrupt temporal record: {0}")]
    Corrupt(String),
    #[error("temporal record does not match its storage key")]
    KeyMismatch,
    #[error("cannot encode temporal state: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error(transparent)]
    State(#[from] TemporalStateError),
}

impl TemporalCodecError {
    pub(super) fn invalid(message: &str) -> Self {
        Self::Corrupt(message.to_owned())
    }
}

fn encode<T: Serialize>(kind: u8, value: &T) -> Result<Vec<u8>, TemporalCodecError> {
    let mut result = Vec::from(MAGIC.as_slice());
    result.extend_from_slice(&CODEC_VERSION.to_be_bytes());
    result.push(kind);
    rmp_serde::encode::write(&mut result, value)?;
    Ok(result)
}

fn frame(bytes: &[u8]) -> Result<(u8, &[u8]), TemporalCodecError> {
    if bytes.len() < MAGIC.len() {
        return Err(TemporalCodecError::invalid("truncated header"));
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(TemporalCodecError::MigrationRequired);
    }
    if bytes.len() < 7 {
        return Err(TemporalCodecError::invalid("truncated header"));
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != CODEC_VERSION {
        return Err(TemporalCodecError::UnsupportedVersion { found: version });
    }
    let kind = bytes[6];
    if !matches!(
        kind,
        1..=7 | VALUE_KIND | TICKET_KIND | KEY_KIND | CATALOG_KIND
    ) {
        return Err(TemporalCodecError::UnsupportedRecord { kind });
    }
    Ok((kind, &bytes[7..]))
}

fn decode<T: DeserializeOwned + Serialize>(
    bytes: &[u8],
    kind: u8,
) -> Result<T, TemporalCodecError> {
    let (actual, payload) = frame(bytes)?;
    if actual != kind {
        return Err(TemporalCodecError::invalid("wrong record kind"));
    }
    let value: T =
        rmp_serde::from_slice(payload).map_err(|e| TemporalCodecError::Corrupt(e.to_string()))?;
    // Reject trailing data, duplicate map entries, and alternate encodings that could alias keys.
    if encode(kind, &value)? != bytes {
        return Err(TemporalCodecError::invalid("noncanonical or trailing data"));
    }
    Ok(value)
}

fn record_kind(record: &TemporalRecord) -> u8 {
    match record {
        TemporalRecord::Epoch(_) => 1,
        TemporalRecord::Origin { .. } => 2,
        TemporalRecord::Input(_) => 3,
        TemporalRecord::GroupOrigin { .. } => 4,
        TemporalRecord::Group(_) => 5,
        TemporalRecord::Cell(_) => 6,
        TemporalRecord::Part(_) => 7,
    }
}

pub fn encode_record(record: &TemporalRecord) -> Result<Vec<u8>, TemporalCodecError> {
    record.validate()?;
    encode(record_kind(record), record)
}

pub fn decode_record(
    key: &TemporalKey,
    bytes: &[u8],
) -> Result<TemporalRecord, TemporalCodecError> {
    let (kind, _) = frame(bytes)?;
    if !(1..=7).contains(&kind) {
        return Err(TemporalCodecError::invalid(
            "expected a temporal index record",
        ));
    }
    let record: TemporalRecord = decode(bytes, kind)?;
    if kind != record_kind(&record) {
        return Err(TemporalCodecError::invalid(
            "record kind disagrees with payload",
        ));
    }
    record.validate()?;
    let stored_namespace = record.key().namespace();
    if stored_namespace.epoch != key.namespace().epoch {
        return Err(TemporalCodecError::KeyMismatch);
    }
    if stored_namespace.plan != key.namespace().plan {
        return Err(TemporalCodecError::PlanVersionMismatch {
            expected: key.namespace().plan.0,
            found: stored_namespace.plan.0,
        });
    }
    if encode_key(&record.key())? != encode_key(key)? {
        return Err(TemporalCodecError::KeyMismatch);
    }
    Ok(record)
}

pub fn encode_value(value: &VariableValue) -> Result<Vec<u8>, TemporalCodecError> {
    encode(VALUE_KIND, &StoredValue::try_from(value)?)
}

pub fn decode_value(bytes: &[u8]) -> Result<VariableValue, TemporalCodecError> {
    decode::<StoredValue>(bytes, VALUE_KIND)?.restore()
}

pub fn encode_ticket(ticket: &FutureTicket) -> Result<Vec<u8>, TemporalCodecError> {
    ticket.validate()?;
    encode(TICKET_KIND, ticket)
}

pub fn decode_ticket(bytes: &[u8]) -> Result<FutureTicket, TemporalCodecError> {
    let ticket: FutureTicket = decode(bytes, TICKET_KIND)?;
    ticket.validate()?;
    Ok(ticket)
}

pub fn encode_catalog(catalog: &TemporalCatalog) -> Result<Vec<u8>, TemporalCodecError> {
    encode(CATALOG_KIND, catalog)
}

pub fn decode_catalog(bytes: &[u8]) -> Result<TemporalCatalog, TemporalCodecError> {
    decode(bytes, CATALOG_KIND)
}

/// Prefix intentionally excludes the codec/plan version so epoch removal covers every format.
pub fn epoch_prefix(epoch: QueryEpoch) -> Vec<u8> {
    let mut prefix = KEY_PREFIX.to_vec();
    prefix.extend_from_slice(&epoch.0);
    prefix
}

#[derive(Serialize)]
enum KeyIdentity<'a> {
    Origin(&'a OriginKey),
    Input(TemporalInputId),
    GroupOrigin {
        namespace: TemporalNamespace,
        producer: PartId,
        grouping: Vec<StoredGroupingValue>,
    },
    Group(TemporalGroupId),
    Cell(&'a FunctionCellId),
    Part {
        namespace: TemporalNamespace,
        part: PartId,
    },
}

pub fn encode_key(key: &TemporalKey) -> Result<Vec<u8>, TemporalCodecError> {
    let namespace = key.namespace();
    let mut bytes = epoch_prefix(namespace.epoch);
    let identity = match key {
        TemporalKey::Epoch(_) => {
            // Discovery must not depend on the codec or plan version being discovered.
            bytes.extend_from_slice(b":manifest");
            return Ok(bytes);
        }
        TemporalKey::Origin(key) => KeyIdentity::Origin(key),
        TemporalKey::Input(id) => KeyIdentity::Input(*id),
        TemporalKey::GroupOrigin(key) => KeyIdentity::GroupOrigin {
            namespace,
            producer: key.producer,
            grouping: key
                .grouping
                .0
                .iter()
                .map(StoredGroupingValue::try_from)
                .collect::<Result<_, _>>()?,
        },
        TemporalKey::Group(id) => KeyIdentity::Group(*id),
        TemporalKey::Cell(id) => KeyIdentity::Cell(id),
        TemporalKey::Part { namespace, part } => KeyIdentity::Part {
            namespace: *namespace,
            part: *part,
        },
    };
    bytes.extend_from_slice(&namespace.plan.0.to_be_bytes());
    bytes.extend_from_slice(&encode(KEY_KIND, &identity)?);
    Ok(bytes)
}

/// Garnet hash fields are strings; hexadecimal is injective and does not depend on UTF-8.
pub fn key_field(key: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut field = String::with_capacity(key.len() * 2);
    for byte in key {
        field.push(char::from(HEX[usize::from(byte >> 4)]));
        field.push(char::from(HEX[usize::from(byte & 15)]));
    }
    field
}

pub(crate) fn decode_field(field: &str) -> Result<Vec<u8>, TemporalCodecError> {
    if !field.len().is_multiple_of(2) {
        return Err(TemporalCodecError::invalid(
            "odd hexadecimal payload length",
        ));
    }
    field
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            fn digit(byte: u8) -> Result<u8, TemporalCodecError> {
                match byte {
                    b'0'..=b'9' => Ok(byte - b'0'),
                    b'a'..=b'f' => Ok(byte - b'a' + 10),
                    _ => Err(TemporalCodecError::invalid("invalid hexadecimal payload")),
                }
            }
            Ok(digit(pair[0])? * 16 + digit(pair[1])?)
        })
        .collect()
}

/// Serialize the entire batch before the first provider mutation. Last write to a key wins.
pub fn prepare_batch(
    batch: &TemporalBatch,
) -> Result<BTreeMap<Vec<u8>, Option<Vec<u8>>>, TemporalCodecError> {
    let mut writes = BTreeMap::new();
    for mutation in batch.mutations() {
        match mutation {
            TemporalMutation::Put(record) => {
                writes.insert(encode_key(&record.key())?, Some(encode_record(record)?));
            }
            TemporalMutation::Delete(key) => {
                writes.insert(encode_key(key)?, None);
            }
        }
    }
    Ok(writes)
}

#[cfg(test)]
mod tests;
