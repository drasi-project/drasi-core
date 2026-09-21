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

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, OnceLock},
};

use bincode::Options;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core::evaluation::context::{QueryPartEvaluationContext, QueryVariables};
use serde::{Deserialize, Serialize};

use crate::{
    channels::QueryResult,
    computation::internal::typed_change::{self, value_codec},
    profiling::ProfilingMetadata,
};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId, ComponentId, ContextEntry,
    ContextValue, ContractError, Record, RecordId, RecordImage, RecordValidationError,
    RecordValidator, Schema, SchemaDescriptor, SchemaId, SchemaVersion, SystemMetadata,
    UpdateSemantics,
};

const QUERY_METADATA: &str = "drasi.query-output.v1";
const QUERY_SEQUENCE: &str = "drasi.query-output-sequence.v1";
const SNAPSHOT: &str = "drasi.query-snapshot.v1";
const GENERATION: &str = "drasi.query-generation.v1";
const QUERY_CORE_RETURN_NS: &str = "drasi.query-core-return-ns.v1";
const QUERY_SEND_NS: &str = "drasi.query-send-ns.v1";

#[derive(Debug, thiserror::Error)]
pub enum QueryCodecError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("invalid typed query row: {0}")]
    Encoding(#[from] Box<bincode::ErrorKind>),
    #[error("invalid query metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("query boundary conversion failed: {0}")]
    Boundary(#[source] anyhow::Error),
    #[error("query update/delete requires its before image")]
    MissingBefore,
    #[error("query row or metadata is incompatible with its envelope")]
    InvalidRow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryOutputMetadata {
    pub query_id: String,
    pub source_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub profiling: Option<ProfilingMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryRowKind {
    Row,
    Aggregation {
        grouping_keys: Vec<String>,
        default_before: bool,
        default_after: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct Row {
    query_id: String,
    signature: u64,
    values: BTreeMap<String, value_codec::Value>,
    kind: QueryRowKind,
}

pub struct DecodedQueryRow {
    pub query_id: String,
    pub signature: u64,
    pub values: QueryVariables,
    pub kind: QueryRowKind,
}

pub struct QueryChangeCodec;

fn namespace(query_id: &str) -> String {
    format!(
        "drasi.query-row/{}",
        query_id
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn decode_row(bytes: &[u8]) -> Result<Row, Box<bincode::ErrorKind>> {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(bytes.len() as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)
}

impl QueryChangeCodec {
    pub fn query_generation(envelope: &ChangeEnvelope) -> Result<u64, QueryCodecError> {
        match envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == GENERATION)
        {
            Some(entry) => match entry.value() {
                ContextValue::Unsigned(generation) => Ok(generation),
                _ => Err(QueryCodecError::InvalidRow),
            },
            None => Ok(0),
        }
    }

    pub(crate) fn set_generation(
        envelope: &mut ChangeEnvelope,
        component: &ComponentId,
        generation: u64,
    ) -> Result<(), QueryCodecError> {
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            GENERATION,
            ContextValue::Unsigned(generation),
        )?)?;
        Ok(())
    }

    pub fn is_snapshot(envelope: &ChangeEnvelope) -> bool {
        envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == SNAPSHOT)
            .is_some_and(|entry| entry.value() == ContextValue::Bool(true))
    }

    pub fn snapshot_envelope(
        query_id: &str,
        snapshot: &super::QuerySnapshot,
        component: &ComponentId,
        system: SystemMetadata,
    ) -> Result<ChangeEnvelope, QueryCodecError> {
        let mut rows: Vec<_> = snapshot.rows.values().cloned().collect();
        rows.sort_by(|left, right| {
            (left.identity().namespace(), left.identity().value())
                .cmp(&(right.identity().namespace(), right.identity().value()))
        });
        let operations = rows
            .into_iter()
            .enumerate()
            .map(|(ordinal, after)| super::ChangeOperation::Added {
                ordinal: ordinal as u64,
                after,
            })
            .collect();
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                system.stream().as_str(),
                Bytes::copy_from_slice(&system.sequence().to_be_bytes()),
            )?,
            Self::schema().descriptor().clone(),
            operations,
        )?;
        let id = super::emission_id(system.stream(), system.sequence())?;
        let mut envelope = ChangeEnvelope::new(id, changes, system);
        let metadata = QueryOutputMetadata {
            query_id: query_id.to_owned(),
            source_id: None,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
            profiling: None,
        };
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            QUERY_METADATA,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&metadata)?)),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            QUERY_SEQUENCE,
            ContextValue::Unsigned(snapshot.as_of_sequence),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            SNAPSHOT,
            ContextValue::Bool(true),
        )?)?;
        Self::set_generation(&mut envelope, component, snapshot.generation)?;
        Ok(envelope)
    }

    pub fn query_sequence(envelope: &ChangeEnvelope) -> Result<u64, QueryCodecError> {
        match envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == QUERY_SEQUENCE)
        {
            Some(entry) => match entry.value() {
                ContextValue::Unsigned(sequence) => Ok(sequence),
                _ => Err(QueryCodecError::InvalidRow),
            },
            // Earlier v1 envelopes used the query producer's system sequence.
            None => Ok(envelope.system().sequence()),
        }
    }

    pub fn schema() -> Arc<Schema> {
        static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
        SCHEMA.get_or_init(|| Arc::new(Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("drasi.query-row").expect("constant schema id"),
                SchemaVersion::try_new(1).expect("constant version"),
                "drasi-query-row-bincode1",
                Bytes::from_static(b"query-row-v1;scoped-signature;typed-values;aggregation-metadata;full-or-partial"),
            ).expect("constant row schema"),
            Arc::new(RowValidator),
        ))).clone()
    }

    pub fn encode_row(
        query_id: &str,
        signature: u64,
        variables: &QueryVariables,
        kind: QueryRowKind,
        image: RecordImage,
    ) -> Result<Record, QueryCodecError> {
        let row = Row {
            query_id: query_id.to_owned(),
            signature,
            values: value_codec::encode_variables(variables).map_err(QueryCodecError::Boundary)?,
            kind,
        };
        Ok(Record::try_new(
            &Self::schema(),
            RecordId::try_new(
                namespace(query_id),
                Bytes::copy_from_slice(&signature.to_be_bytes()),
            )?,
            image,
            Bytes::from(bincode::serialize(&row)?),
        )?)
    }

    pub fn decode_row(record: &Record) -> Result<DecodedQueryRow, QueryCodecError> {
        super::data::validate_schema(Self::schema().descriptor(), record.schema())?;
        let row = decode_row(record.payload())?;
        Ok(DecodedQueryRow {
            query_id: row.query_id,
            signature: row.signature,
            values: value_codec::decode_variables(row.values).map_err(QueryCodecError::Boundary)?,
            kind: row.kind,
        })
    }

    pub fn metadata(envelope: &ChangeEnvelope) -> Result<QueryOutputMetadata, QueryCodecError> {
        let mut core_return_ns = None;
        let mut send_ns = None;
        for entry in envelope.annotations().entries() {
            match entry.key() {
                QUERY_CORE_RETURN_NS | QUERY_SEND_NS => {
                    let ContextValue::Unsigned(timestamp) = entry.value() else {
                        return Err(QueryCodecError::InvalidRow);
                    };
                    let latest = if entry.key() == QUERY_CORE_RETURN_NS {
                        &mut core_return_ns
                    } else {
                        &mut send_ns
                    };
                    latest.get_or_insert(timestamp);
                }
                QUERY_METADATA => {
                    let ContextValue::Bytes(bytes) = entry.value() else {
                        return Err(QueryCodecError::InvalidRow);
                    };
                    let mut metadata: QueryOutputMetadata = serde_json::from_slice(&bytes)?;
                    if core_return_ns.is_some() || send_ns.is_some() {
                        let profiling = metadata
                            .profiling
                            .get_or_insert_with(ProfilingMetadata::default);
                        if let Some(timestamp) = core_return_ns {
                            profiling.query_core_return_ns = Some(timestamp);
                        }
                        if let Some(timestamp) = send_ns {
                            profiling.query_send_ns = Some(timestamp);
                        }
                    }
                    // Older annotations belong to an input, not this query's output.
                    return Ok(metadata);
                }
                _ => {}
            }
        }
        Err(QueryCodecError::InvalidRow)
    }

    pub(crate) fn append_post_commit_profiling(
        envelope: &mut ChangeEnvelope,
        component: &ComponentId,
        core_return_ns: u64,
        send_ns: u64,
    ) -> Result<(), QueryCodecError> {
        for (key, timestamp) in [(QUERY_CORE_RETURN_NS, core_return_ns), (QUERY_SEND_NS, send_ns)] {
            envelope.append_annotation(ContextEntry::try_new(
                component.clone(),
                key,
                ContextValue::Unsigned(timestamp),
            )?)?;
        }
        Ok(())
    }

    pub fn encode_evaluation(
        input: Option<&ChangeEnvelope>,
        component: &ComponentId,
        system: SystemMetadata,
        results: &[QueryPartEvaluationContext],
        metadata: QueryOutputMetadata,
    ) -> Result<Option<ChangeEnvelope>, QueryCodecError> {
        let typed = typed_change::query_evaluation_to_envelope(
            results,
            typed_change::QueryEnvelopeMetadata::new(
                metadata.query_id.clone(),
                metadata.source_id.as_deref().map(Arc::from),
                system.sequence(),
                metadata.timestamp,
                metadata.metadata.clone(),
                metadata.profiling.clone(),
            ),
        )
        .map_err(|error| QueryCodecError::Boundary(error.into()))?;
        let Some(typed) = typed else { return Ok(None) };
        let mut operations = Vec::new();
        let variables = |record: &typed_change::RecordData| {
            if let typed_change::RecordData::QueryRow(values) = record {
                Ok(values.clone())
            } else {
                Err(QueryCodecError::InvalidRow)
            }
        };
        let signature = |identity: &typed_change::RecordIdentity| {
            if let typed_change::RecordIdentity::QueryRow(signature) = identity {
                Ok(*signature)
            } else {
                Err(QueryCodecError::InvalidRow)
            }
        };
        for record in typed.change_set().added() {
            operations.push(ChangeOperation::Added {
                ordinal: record.ordinal() as u64,
                after: Self::encode_row(
                    &metadata.query_id,
                    signature(record.identity())?,
                    variables(record.after())?.as_ref(),
                    QueryRowKind::Row,
                    RecordImage::Full,
                )?,
            });
        }
        for record in typed.change_set().updated() {
            let kind = match record.metadata() {
                typed_change::UpdateMetadata::QueryAggregation {
                    grouping_keys,
                    default_before,
                    default_after,
                } => QueryRowKind::Aggregation {
                    grouping_keys: grouping_keys.iter().map(|key| key.to_string()).collect(),
                    default_before: *default_before,
                    default_after: *default_after,
                },
                typed_change::UpdateMetadata::QueryUpdate { .. } => QueryRowKind::Row,
                typed_change::UpdateMetadata::None => return Err(QueryCodecError::InvalidRow),
            };
            let signature = signature(record.identity())?;
            let before = record
                .before()
                .map(|before| {
                    Self::encode_row(
                        &metadata.query_id,
                        signature,
                        variables(before)?.as_ref(),
                        kind.clone(),
                        RecordImage::Full,
                    )
                })
                .transpose()?;
            operations.push(ChangeOperation::Updated {
                ordinal: record.ordinal() as u64,
                before,
                after: Self::encode_row(
                    &metadata.query_id,
                    signature,
                    variables(record.after())?.as_ref(),
                    kind,
                    RecordImage::Full,
                )?,
                semantics: UpdateSemantics::Replace,
            });
        }
        for record in typed.change_set().deleted() {
            let before = Self::encode_row(
                &metadata.query_id,
                signature(record.identity())?,
                variables(record.before().ok_or(QueryCodecError::MissingBefore)?)?.as_ref(),
                QueryRowKind::Row,
                RecordImage::Full,
            )?;
            operations.push(ChangeOperation::Deleted {
                ordinal: record.ordinal() as u64,
                identity: before.reference().clone(),
                before: Some(before),
            });
        }
        operations.sort_by_key(ChangeOperation::ordinal);
        let sequence = system.sequence();
        let id = super::emission_id(system.stream(), sequence)?;
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                system.stream().as_str(),
                Bytes::copy_from_slice(&system.sequence().to_be_bytes()),
            )?,
            Self::schema().descriptor().clone(),
            operations,
        )?;
        let mut envelope = match input {
            Some(input) => input.derive(id, changes, system),
            None => ChangeEnvelope::new(id, changes, system),
        };
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            QUERY_SEQUENCE,
            ContextValue::Unsigned(sequence),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            SNAPSHOT,
            ContextValue::Bool(false),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            QUERY_METADATA,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&metadata)?)),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            "drasi.advisory-query-change-identity",
            ContextValue::Unsigned(typed.change_set().id().value()),
        )?)?;
        Ok(Some(envelope))
    }

    pub fn decode_evaluation(
        envelope: &ChangeEnvelope,
    ) -> Result<Vec<QueryPartEvaluationContext>, QueryCodecError> {
        super::data::validate_schema(Self::schema().descriptor(), envelope.changes().schema())?;
        envelope
            .changes()
            .operations()
            .iter()
            .map(|operation| {
                Ok(match operation {
                    ChangeOperation::Added { after, .. } => {
                        let after = Self::decode_row(after)?;
                        QueryPartEvaluationContext::Adding {
                            after: after.values,
                            row_signature: after.signature,
                        }
                    }
                    ChangeOperation::Updated {
                        before,
                        after,
                        semantics: UpdateSemantics::Replace,
                        ..
                    } => {
                        let after = Self::decode_row(after)?;
                        let before = before.as_ref().map(Self::decode_row).transpose()?;
                        if before
                            .as_ref()
                            .is_some_and(|before| before.kind != after.kind)
                        {
                            return Err(QueryCodecError::InvalidRow);
                        }
                        match after.kind {
                            QueryRowKind::Row => QueryPartEvaluationContext::Updating {
                                before: before.ok_or(QueryCodecError::MissingBefore)?.values,
                                after: after.values,
                                row_signature: after.signature,
                            },
                            QueryRowKind::Aggregation {
                                grouping_keys,
                                default_before,
                                default_after,
                            } => QueryPartEvaluationContext::Aggregation {
                                before: before.map(|before| before.values),
                                after: after.values,
                                grouping_keys,
                                default_before,
                                default_after,
                                row_signature: after.signature,
                            },
                        }
                    }
                    ChangeOperation::Deleted { before, .. } => {
                        let before = Self::decode_row(
                            before.as_ref().ok_or(QueryCodecError::MissingBefore)?,
                        )?;
                        QueryPartEvaluationContext::Removing {
                            before: before.values,
                            row_signature: before.signature,
                        }
                    }
                    _ => return Err(QueryCodecError::InvalidRow),
                })
            })
            .collect()
    }

    /// Explicit projection into the unchanged legacy query-result boundary.
    /// The original A2 projection code is executed, not copied into a new facade.
    pub fn to_legacy_result(envelope: &ChangeEnvelope) -> Result<QueryResult, QueryCodecError> {
        let metadata = Self::metadata(envelope)?;
        if envelope
            .changes()
            .operations()
            .iter()
            .any(|operation| operation.identity().namespace() != namespace(&metadata.query_id))
        {
            return Err(QueryCodecError::InvalidRow);
        }
        let results = Self::decode_evaluation(envelope)?;
        let typed = typed_change::query_evaluation_to_envelope(
            &results,
            typed_change::QueryEnvelopeMetadata::new(
                metadata.query_id,
                metadata.source_id.map(Arc::from),
                Self::query_sequence(envelope)?,
                metadata.timestamp,
                metadata.metadata,
                metadata.profiling,
            ),
        )
        .map_err(|error| QueryCodecError::Boundary(error.into()))?
        .ok_or(QueryCodecError::InvalidRow)?;
        typed_change::query_result_from_envelope(&typed)
            .map_err(|error| QueryCodecError::Boundary(error.into()))
    }
}

struct RowValidator;

impl RecordValidator for RowValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> Result<(), RecordValidationError> {
        let scope = identity
            .namespace()
            .strip_prefix("drasi.query-row/")
            .unwrap_or("");
        if schema != QueryChangeCodec::schema().descriptor()
            || scope.is_empty()
            || scope.len() % 2 != 0
            || !scope
                .bytes()
                .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
            || identity.value().len() != 8
        {
            return Err(RecordValidationError::new(
                "identity",
                "expected scoped query row signature",
            ));
        }
        let decoded: Vec<_> = scope
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |value: u8| {
                    if value <= b'9' {
                        value - b'0'
                    } else {
                        value - b'a' + 10
                    }
                };
                digit(pair[0]) * 16 + digit(pair[1])
            })
            .collect();
        std::str::from_utf8(&decoded)
            .map_err(|error| RecordValidationError::new("identity", error.to_string()))?;
        Ok(())
    }
    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> Result<(), RecordValidationError> {
        self.validate_identity(schema, identity)?;
        let row = decode_row(payload)
            .map_err(|error| RecordValidationError::new("encoding", error.to_string()))?;
        if image == RecordImage::Patch
            || identity.namespace() != namespace(&row.query_id)
            || identity.value().as_ref() != row.signature.to_be_bytes()
        {
            return Err(RecordValidationError::new(
                "image",
                "query row identity or image mismatch",
            ));
        }
        value_codec::decode_variables::<String>(row.values)
            .map_err(|error| RecordValidationError::new("value", error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod profiling_tests {
    use super::*;
    use drasi_core::evaluation::variable_value::VariableValue;
    use std::num::NonZeroUsize;

    fn output(
        input: Option<&ChangeEnvelope>,
        query: &str,
        profiling: Option<ProfilingMetadata>,
    ) -> ChangeEnvelope {
        QueryChangeCodec::encode_evaluation(
            input,
            &ComponentId::try_new(query).unwrap(),
            SystemMetadata::new(
                super::super::StreamId::try_new(format!("{query}/out")).unwrap(),
                1,
            ),
            &[QueryPartEvaluationContext::Adding {
                after: BTreeMap::from([("value".into(), VariableValue::from(1))]),
                row_signature: 7,
            }],
            QueryOutputMetadata {
                query_id: query.into(),
                source_id: Some("source".into()),
                timestamp: DateTime::from_timestamp_millis(1_000).unwrap(),
                metadata: HashMap::from([("preserved".into(), serde_json::json!(true))]),
                profiling,
            },
        )
        .unwrap()
        .unwrap()
    }

    fn metadata_bytes(envelope: &ChangeEnvelope) -> Arc<[u8]> {
        let entry = envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == QUERY_METADATA)
            .unwrap();
        let ContextValue::Bytes(bytes) = entry.value() else {
            panic!("query metadata bytes");
        };
        bytes
    }

    #[test]
    fn post_commit_profiling_is_additive_and_round_trips_without_rewriting_metadata() {
        let mut expected = ProfilingMetadata {
            source_ns: Some(11),
            source_receive_ns: Some(12),
            source_send_ns: Some(13),
            query_receive_ns: Some(14),
            query_core_call_ns: Some(15),
            ..Default::default()
        };
        let mut live = output(None, "query", Some(expected.clone()));
        let staged = live.clone();
        let original_bytes = metadata_bytes(&staged);
        QueryChangeCodec::append_post_commit_profiling(
            &mut live,
            &ComponentId::try_new("query").unwrap(),
            16,
            17,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&metadata_bytes(&live), &original_bytes));
        assert!(Arc::ptr_eq(live.event(), staged.event()));
        assert_eq!(live.annotations().len(), staged.annotations().len() + 2);
        assert_eq!(
            QueryChangeCodec::metadata(&staged).unwrap().profiling,
            Some(expected.clone())
        );
        expected.query_core_return_ns = Some(16);
        expected.query_send_ns = Some(17);
        assert_eq!(
            QueryChangeCodec::metadata(&live).unwrap().profiling,
            Some(expected.clone())
        );
        assert_eq!(
            QueryChangeCodec::to_legacy_result(&live).unwrap().profiling,
            Some(expected.clone())
        );

        let mut codec = super::super::EnvelopeCodec::new(NonZeroUsize::new(64 * 1024).unwrap());
        codec.register_schema(QueryChangeCodec::schema()).unwrap();
        let decoded = codec.decode(&codec.encode(&live).unwrap()).unwrap();
        assert_eq!(
            QueryChangeCodec::metadata(&decoded).unwrap().profiling,
            Some(expected)
        );
        assert_eq!(metadata_bytes(&decoded), original_bytes);
    }

    #[test]
    fn current_metadata_fences_inherited_profiling_and_newest_local_values_win() {
        let contributor = ComponentId::try_new("upstream").unwrap();
        let mut upstream = output(None, "upstream", None);
        QueryChangeCodec::append_post_commit_profiling(&mut upstream, &contributor, 10, 20)
            .unwrap();
        upstream
            .append_annotation(
                ContextEntry::try_new(
                    contributor,
                    QUERY_SEND_NS,
                    ContextValue::String("invalid inherited timing".into()),
                )
                .unwrap(),
            )
            .unwrap();

        let mut downstream = output(Some(&upstream), "downstream", None);
        assert!(QueryChangeCodec::metadata(&downstream)
            .unwrap()
            .profiling
            .is_none());
        let contributor = ComponentId::try_new("downstream").unwrap();
        downstream
            .append_annotation(
                ContextEntry::try_new(
                    contributor.clone(),
                    QUERY_CORE_RETURN_NS,
                    ContextValue::Unsigned(30),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            QueryChangeCodec::metadata(&downstream).unwrap().profiling,
            Some(ProfilingMetadata {
                query_core_return_ns: Some(30),
                ..Default::default()
            }),
            "neither upstream send time nor invented source/start timestamps may leak"
        );
        QueryChangeCodec::append_post_commit_profiling(&mut downstream, &contributor, 40, 50)
            .unwrap();
        assert_eq!(
            QueryChangeCodec::metadata(&downstream).unwrap().profiling,
            Some(ProfilingMetadata {
                query_core_return_ns: Some(40),
                query_send_ns: Some(50),
                ..Default::default()
            })
        );
    }

    #[test]
    fn malformed_local_profiling_values_are_rejected() {
        for key in [QUERY_CORE_RETURN_NS, QUERY_SEND_NS] {
            for value in [
                ContextValue::Signed(-1),
                ContextValue::Signed(1),
                ContextValue::Bool(false),
                ContextValue::String("123".into()),
                ContextValue::Bytes(Arc::from(&b"123"[..])),
            ] {
                let mut envelope = output(None, "query", None);
                envelope
                    .append_annotation(
                        ContextEntry::try_new(ComponentId::try_new("query").unwrap(), key, value)
                            .unwrap(),
                    )
                    .unwrap();
                assert!(matches!(
                    QueryChangeCodec::metadata(&envelope),
                    Err(QueryCodecError::InvalidRow)
                ));
                assert!(QueryChangeCodec::to_legacy_result(&envelope).is_err());
            }
        }
    }

    #[test]
    fn historical_embedded_profiling_remains_readable_without_new_annotations() {
        let expected = ProfilingMetadata {
            source_receive_ns: Some(1),
            query_receive_ns: Some(2),
            query_core_call_ns: Some(3),
            query_core_return_ns: Some(4),
            query_send_ns: Some(5),
            ..Default::default()
        };
        let envelope = output(None, "query", Some(expected.clone()));
        assert_eq!(
            QueryChangeCodec::metadata(&envelope).unwrap().profiling,
            Some(expected)
        );
        assert!(QueryChangeCodec::metadata(&output(None, "query", None))
            .unwrap()
            .profiling
            .is_none());
    }
}
