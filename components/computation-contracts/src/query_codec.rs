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

//! Query output on the ordinary graph-change contract.
//!
//! A query row is a standard graph node (`ElementImage::Node`) published with
//! [`GraphChangeCodec`]; generic graph consumers need nothing from this module.
//! Query-facing facts that are not node data (row kind, grouping keys, default
//! flags, ordinals) travel in one projection-context annotation scoped to the
//! logical change set it was written for. An inherited annotation from an
//! upstream output is therefore never read as the current output's context.

use std::{collections::HashMap, sync::Arc};

use bincode::Options;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core_types::evaluation::context::{QueryPartEvaluationContext, QueryVariables};
use serde::{Deserialize, Serialize};

use crate::{
    exchange::QueryResult, internal::typed_change, profiling::ProfilingMetadata, ElementImage,
    GraphChangeCodec, GraphCodecError, ResultNodeError, ResultNodeProjector,
    ResultOperationMetadata,
};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeOrdering, ChangeSet, ChangeSetId, ComponentId,
    ContextEntry, ContextValue, ContractError, Record, RecordId, Schema, SystemMetadata,
    UpdateSemantics,
};

pub const QUERY_METADATA: &str = "drasi.query-output.v1";
pub const QUERY_PROJECTION: &str = "drasi.query-projection.v1";
const QUERY_SEQUENCE: &str = "drasi.query-output-sequence.v1";
const SNAPSHOT: &str = "drasi.query-snapshot.v1";
const PROGRESS_ONLY: &str = "drasi.query-progress-only.v1";
const GENERATION: &str = "drasi.query-generation.v1";
const QUERY_CORE_RETURN_NS: &str = "drasi.query-core-return-ns.v1";
const QUERY_SEND_NS: &str = "drasi.query-send-ns.v1";
const PROJECTION_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum QueryCodecError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("invalid query projection context: {0}")]
    Encoding(#[from] Box<bincode::ErrorKind>),
    #[error("invalid query metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("query boundary conversion failed: {0}")]
    Boundary(#[source] anyhow::Error),
    #[error("query update/delete requires its before image")]
    MissingBefore,
    #[error("query row or metadata is incompatible with its envelope")]
    InvalidRow,
    #[error("query output time must not precede the Unix epoch")]
    InvalidEventTime,
    #[error(transparent)]
    Node(#[from] ResultNodeError),
    #[error(transparent)]
    Graph(Box<GraphCodecError>),
}

impl From<GraphCodecError> for QueryCodecError {
    fn from(error: GraphCodecError) -> Self {
        Self::Graph(Box::new(error))
    }
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

/// Query-facing row classification. It is envelope context, never node data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryRowKind {
    Row,
    Aggregation {
        grouping_keys: Vec<String>,
        default_before: bool,
        default_after: bool,
    },
}

/// A result node read back without any query-specific wire fields.
///
/// `dataset`/`label` come from the node itself (or the supplied scope), and
/// `identity` is the opaque row key. `signature` is present only when the key is
/// exactly an evaluator row signature. The query ID is not recoverable from a
/// record; it belongs to the scoped envelope metadata.
#[derive(Debug, Clone)]
pub struct DecodedQueryRow {
    pub dataset: String,
    pub label: String,
    pub identity: Bytes,
    pub signature: Option<u64>,
    pub values: QueryVariables,
}

/// The current query projection carried by an envelope: the output scope and one
/// index-aligned [`ResultOperationMetadata`] per change operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryProjection {
    projector: ResultNodeProjector,
    operations: Vec<ResultOperationMetadata>,
}

impl QueryProjection {
    pub fn projector(&self) -> &ResultNodeProjector {
        &self.projector
    }

    /// `operations()[i]` describes `envelope.changes().operations()[i]`.
    pub fn operations(&self) -> &[ResultOperationMetadata] {
        &self.operations
    }
}

// `ProjectionContext` starts with `ProjectionScope`, so the scope can be read as
// a prefix without decoding every operation.
#[derive(Serialize, Deserialize)]
struct ProjectionScope {
    version: u32,
    change_set_namespace: String,
    change_set_value: Vec<u8>,
    query_id: String,
}

#[derive(Serialize, Deserialize)]
struct ProjectionOperation {
    ordinal: u64,
    row_identity: Vec<u8>,
    kind: QueryRowKind,
}

#[derive(Serialize, Deserialize)]
struct ProjectionContext {
    scope: ProjectionScope,
    dataset: String,
    label: String,
    operations: Vec<ProjectionOperation>,
}

fn encode_context(context: &ProjectionContext) -> Result<Vec<u8>, Box<bincode::ErrorKind>> {
    bincode::options().with_fixint_encoding().serialize(context)
}

fn decode_context(bytes: &[u8]) -> Result<ProjectionContext, Box<bincode::ErrorKind>> {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(bytes.len() as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)
}

fn decode_scope(bytes: &[u8]) -> Result<ProjectionScope, Box<bincode::ErrorKind>> {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(bytes.len() as u64)
        .allow_trailing_bytes()
        .deserialize(bytes)
}

struct RecoveryPayload {
    operations: Vec<ChangeOperation>,
    metadata: Vec<ResultOperationMetadata>,
    marker: &'static str,
}

pub struct QueryChangeCodec;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SNAPSHOT_ROW_PROJECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl QueryChangeCodec {
    /// Query output uses the ordinary graph-change schema.
    pub fn schema() -> Arc<Schema> {
        GraphChangeCodec::schema()
    }

    /// Whether the newest projection annotation was written for this envelope's
    /// own change set and agrees with its operations. A schema alone, an
    /// inherited annotation, or a malformed annotation is not a query output.
    pub fn is_query_output(envelope: &ChangeEnvelope) -> bool {
        Self::projection(envelope).is_ok()
    }

    /// The current query projection, or an error if the envelope is not a current
    /// valid query output.
    pub fn projection(envelope: &ChangeEnvelope) -> Result<QueryProjection, QueryCodecError> {
        if !Self::is_current(envelope)? {
            return Err(QueryCodecError::InvalidRow);
        }
        let bytes = Self::newest_projection(envelope)?.ok_or(QueryCodecError::InvalidRow)?;
        let context = decode_context(&bytes)?;
        let projector =
            ResultNodeProjector::try_new(&context.dataset, Some(context.label.as_str()))?;
        let operations = envelope.changes().operations();
        if context.operations.len() != operations.len() {
            return Err(QueryCodecError::InvalidRow);
        }
        let mut metadata = Vec::with_capacity(operations.len());
        for (operation, expected) in operations.iter().zip(context.operations) {
            let reference = projector.reference(&expected.row_identity)?;
            if expected.ordinal != operation.ordinal()
                || GraphChangeCodec::element_reference(&reference)?.identity()
                    != operation.identity()
            {
                return Err(QueryCodecError::InvalidRow);
            }
            metadata.push(ResultOperationMetadata {
                ordinal: expected.ordinal,
                row_identity: Bytes::from(expected.row_identity),
                kind: expected.kind,
            });
        }
        Ok(QueryProjection {
            projector,
            operations: metadata,
        })
    }

    fn newest_projection(envelope: &ChangeEnvelope) -> Result<Option<Arc<[u8]>>, QueryCodecError> {
        match envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == QUERY_PROJECTION)
        {
            None => Ok(None),
            Some(entry) => match entry.value() {
                ContextValue::Bytes(bytes) => Ok(Some(bytes)),
                _ => Err(QueryCodecError::InvalidRow),
            },
        }
    }

    /// `Ok(false)` for an absent or inherited projection (a different logical
    /// change set); an error for a malformed or unknown-version one.
    fn is_current(envelope: &ChangeEnvelope) -> Result<bool, QueryCodecError> {
        let Some(bytes) = Self::newest_projection(envelope)? else {
            return Ok(false);
        };
        let scope = decode_scope(&bytes)?;
        if scope.version != PROJECTION_VERSION {
            return Err(QueryCodecError::InvalidRow);
        }
        let id = envelope.changes().id();
        Ok(
            envelope.changes().schema() == GraphChangeCodec::schema().descriptor()
                && scope.change_set_namespace == id.namespace()
                && scope.change_set_value.as_slice() == &id.value()[..],
        )
    }

    /// Annotations contributed for the current output, newest first. Everything
    /// older than the current projection annotation is inherited.
    fn current_entries(envelope: &ChangeEnvelope) -> Result<Vec<ContextEntry>, QueryCodecError> {
        if !Self::is_current(envelope)? {
            return Err(QueryCodecError::InvalidRow);
        }
        Ok(envelope
            .annotations()
            .entries()
            .take_while(|entry| entry.key() != QUERY_PROJECTION)
            .collect())
    }

    fn projection_entry(
        component: &ComponentId,
        id: &ChangeSetId,
        query_id: &str,
        projector: &ResultNodeProjector,
        operations: &[ResultOperationMetadata],
    ) -> Result<ContextEntry, QueryCodecError> {
        let context = ProjectionContext {
            scope: ProjectionScope {
                version: PROJECTION_VERSION,
                change_set_namespace: id.namespace().to_owned(),
                change_set_value: id.value().to_vec(),
                query_id: query_id.to_owned(),
            },
            dataset: projector.dataset().to_owned(),
            label: projector.label().to_owned(),
            operations: operations
                .iter()
                .map(|operation| ProjectionOperation {
                    ordinal: operation.ordinal,
                    row_identity: operation.row_identity.to_vec(),
                    kind: operation.kind.clone(),
                })
                .collect(),
        };
        Ok(ContextEntry::try_new(
            component.clone(),
            QUERY_PROJECTION,
            ContextValue::Bytes(Arc::from(encode_context(&context)?)),
        )?)
    }

    pub fn query_generation(envelope: &ChangeEnvelope) -> Result<u64, QueryCodecError> {
        let entries = Self::current_entries(envelope)?;
        match entries.iter().find(|entry| entry.key() == GENERATION) {
            Some(entry) => match entry.value() {
                ContextValue::Unsigned(generation) => Ok(generation),
                _ => Err(QueryCodecError::InvalidRow),
            },
            None => Ok(0),
        }
    }

    pub fn set_generation(
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

    fn current_marker(envelope: &ChangeEnvelope, marker: &str) -> bool {
        Self::current_entries(envelope).is_ok_and(|entries| {
            entries
                .iter()
                .find(|entry| entry.key() == marker)
                .is_some_and(|entry| entry.value() == ContextValue::Bool(true))
        })
    }

    pub fn is_snapshot(envelope: &ChangeEnvelope) -> bool {
        Self::current_marker(envelope, SNAPSHOT)
    }

    /// Whether an envelope carries the explicit progress-only marker.
    ///
    /// Recovery consumers must also validate the empty change set and their
    /// recovery-decision authorization before checkpointing it.
    pub fn is_progress_only(envelope: &ChangeEnvelope) -> bool {
        Self::current_marker(envelope, PROGRESS_ONLY)
    }

    /// Describe an explicit recovery skip without pretending to replace rows.
    /// This is not a handled checkpoint until a recovery-aware sink accepts it.
    /// The explicit output scope is required even though no rows are carried.
    pub fn progress_envelope(
        projector: &ResultNodeProjector,
        query_id: &str,
        query_sequence: u64,
        generation: u64,
        component: &ComponentId,
        system: SystemMetadata,
    ) -> Result<ChangeEnvelope, QueryCodecError> {
        Self::recovery_envelope(
            projector,
            query_id,
            query_sequence,
            generation,
            component,
            system,
            RecoveryPayload {
                operations: Vec::new(),
                metadata: Vec::new(),
                marker: PROGRESS_ONLY,
            },
        )
    }

    /// Replace-all snapshot of ordinary graph node records for an explicit output
    /// scope. The scope is never inferred from the first row, so an empty
    /// snapshot is fully scoped. Every row must be a full result node of this
    /// scope, keyed by its own identity.
    pub fn snapshot_envelope(
        projector: &ResultNodeProjector,
        query_id: &str,
        snapshot: &super::QueryResultSnapshot,
        component: &ComponentId,
        system: SystemMetadata,
    ) -> Result<ChangeEnvelope, QueryCodecError> {
        if projector != &snapshot.projector {
            return Err(QueryCodecError::InvalidRow);
        }
        let mut rows = Vec::with_capacity(snapshot.rows.len());
        for (identity, record) in &snapshot.rows {
            if identity != record.identity() {
                return Err(QueryCodecError::InvalidRow);
            }
            rows.push(record.clone());
        }
        rows.sort_by(|left, right| {
            (left.identity().namespace(), left.identity().value())
                .cmp(&(right.identity().namespace(), right.identity().value()))
        });
        let mut operations = Vec::with_capacity(rows.len());
        let mut metadata = Vec::with_capacity(rows.len());
        for (ordinal, after) in rows.into_iter().enumerate() {
            let ordinal = ordinal as u64;
            let (row_identity, _) = projector.decode_row(&after)?;
            metadata.push(ResultOperationMetadata {
                ordinal,
                row_identity,
                kind: QueryRowKind::Row,
            });
            operations.push(ChangeOperation::Added { ordinal, after });
        }
        Self::recovery_envelope(
            projector,
            query_id,
            snapshot.as_of_sequence,
            snapshot.generation,
            component,
            system,
            RecoveryPayload {
                operations,
                metadata,
                marker: SNAPSHOT,
            },
        )
    }

    fn recovery_envelope(
        projector: &ResultNodeProjector,
        query_id: &str,
        query_sequence: u64,
        generation: u64,
        component: &ComponentId,
        system: SystemMetadata,
        payload: RecoveryPayload,
    ) -> Result<ChangeEnvelope, QueryCodecError> {
        let system = system.with_ordering(ChangeOrdering::ProducerSequence);
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                system.stream().as_str(),
                Bytes::copy_from_slice(&system.sequence().to_be_bytes()),
            )?,
            GraphChangeCodec::schema().descriptor().clone(),
            payload.operations,
        )?;
        let projection = Self::projection_entry(
            component,
            changes.id(),
            query_id,
            projector,
            &payload.metadata,
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
        envelope.append_annotation(projection)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            QUERY_METADATA,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&metadata)?)),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            QUERY_SEQUENCE,
            ContextValue::Unsigned(query_sequence),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            payload.marker,
            ContextValue::Bool(true),
        )?)?;
        Self::set_generation(&mut envelope, component, generation)?;
        Ok(envelope)
    }

    /// The query's own output sequence. It is explicit in every current output;
    /// there is no fallback to the producer's transport sequence.
    pub fn query_sequence(envelope: &ChangeEnvelope) -> Result<u64, QueryCodecError> {
        let entries = Self::current_entries(envelope)?;
        match entries.iter().find(|entry| entry.key() == QUERY_SEQUENCE) {
            Some(entry) => match entry.value() {
                ContextValue::Unsigned(sequence) => Ok(sequence),
                _ => Err(QueryCodecError::InvalidRow),
            },
            None => Err(QueryCodecError::InvalidRow),
        }
    }

    /// The positive output sequence contributed by this query's own component.
    pub fn output_sequence(envelope: &ChangeEnvelope) -> Result<u64, QueryCodecError> {
        let query = Self::metadata(envelope)?.query_id;
        let entries = Self::current_entries(envelope)?;
        let entry = entries
            .iter()
            .find(|entry| entry.key() == QUERY_SEQUENCE)
            .ok_or(QueryCodecError::InvalidRow)?;
        match entry.value() {
            ContextValue::Unsigned(sequence) if sequence > 0 && entry.contributor() == query => {
                Ok(sequence)
            }
            _ => Err(QueryCodecError::InvalidRow),
        }
    }

    /// Encode one full result node for an explicit output scope.
    /// `effective_from` is business time in milliseconds, not an output sequence.
    pub fn encode_row(
        projector: &ResultNodeProjector,
        signature: u64,
        variables: &QueryVariables,
        effective_from: u64,
    ) -> Result<Record, QueryCodecError> {
        Ok(projector.record(
            &ResultNodeProjector::signature_identity(signature),
            variables,
            effective_from,
        )?)
    }

    /// Read a result node using its own dataset and label. The record carries no
    /// query ID, row kind or signature field; `signature` is derived from an
    /// eight-byte opaque identity. Use [`Self::decode_row_in`] to also require a
    /// particular output scope.
    pub fn decode_row(record: &Record) -> Result<DecodedQueryRow, QueryCodecError> {
        crate::data::validate_schema(GraphChangeCodec::schema().descriptor(), record.schema())?;
        let ElementImage::Node { metadata, .. } = GraphChangeCodec::decode_element(record)? else {
            return Err(QueryCodecError::InvalidRow);
        };
        let [label] = &*metadata.labels else {
            return Err(QueryCodecError::InvalidRow);
        };
        let projector =
            ResultNodeProjector::try_new(&metadata.reference.source_id, Some(&**label))?;
        Self::decode_row_in(&projector, record)
    }

    /// Read a result node and require that it belongs to `projector`'s dataset
    /// and label.
    pub fn decode_row_in(
        projector: &ResultNodeProjector,
        record: &Record,
    ) -> Result<DecodedQueryRow, QueryCodecError> {
        crate::data::validate_schema(GraphChangeCodec::schema().descriptor(), record.schema())?;
        let (identity, values) = projector.decode_row(record)?;
        Ok(DecodedQueryRow {
            dataset: projector.dataset().to_owned(),
            label: projector.label().to_owned(),
            signature: Self::row_signature(&identity),
            identity,
            values,
        })
    }

    /// The evaluator row signature an opaque row identity was derived from.
    pub fn row_signature(identity: &[u8]) -> Option<u64> {
        <[u8; 8]>::try_from(identity).ok().map(u64::from_be_bytes)
    }

    /// Project typed row values to the established query JSON representation
    /// without constructing a legacy QueryResult or source/reaction adapter.
    pub fn row_values_to_json(values: &QueryVariables) -> serde_json::Value {
        typed_change::query_variables_to_json(values)
    }

    fn decode_snapshot_row(
        projector: &ResultNodeProjector,
        record: &Record,
    ) -> Result<DecodedQueryRow, QueryCodecError> {
        let row = Self::decode_row_in(projector, record)?;
        if row.signature.is_none() {
            return Err(QueryCodecError::InvalidRow);
        }
        Ok(row)
    }

    /// Validate immutable encoded rows before crossing the infallible legacy
    /// stream boundary. Validation uses one decoded row at a time; JSON
    /// projection remains demand-driven, including for capped consumers.
    ///
    /// This scans all encoded rows before exposing the iterator. It retains
    /// those same immutable records and an owned scope, so repeating the
    /// deterministic decoder cannot introduce a new input error. An invariant
    /// violation fails loudly rather than becoming EOF or an omitted row.
    pub fn legacy_snapshot_rows(
        projector: &ResultNodeProjector,
        rows: im::HashMap<RecordId, Record>,
    ) -> Result<impl Iterator<Item = (u64, serde_json::Value)> + Send + 'static, QueryCodecError>
    {
        for (identity, record) in &rows {
            if identity != record.identity() {
                return Err(QueryCodecError::InvalidRow);
            }
            Self::decode_snapshot_row(projector, record)?;
        }
        let projector = projector.clone();
        Ok(rows.into_iter().map(move |(_, record)| {
            // The entire immutable snapshot was checked above. This is the
            // same deterministic decoder over the same records, not a deferred
            // fallible fetch that could silently truncate the public stream.
            let row = Self::decode_snapshot_row(&projector, &record)
                .expect("immutable snapshot row passed preflight decoding");
            #[cfg(any(test, feature = "test-support"))]
            SNAPSHOT_ROW_PROJECTIONS.with(|count| count.set(count.get() + 1));
            (
                row.signature
                    .expect("preflight decoding requires a row signature"),
                typed_change::query_variables_to_json(&row.values),
            )
        }))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn snapshot_row_projections() -> usize {
        SNAPSHOT_ROW_PROJECTIONS.with(std::cell::Cell::get)
    }

    /// Metadata of the current output. Annotations inherited from an upstream
    /// output are never returned; an envelope that is not a current query output
    /// is an error.
    pub fn metadata(envelope: &ChangeEnvelope) -> Result<QueryOutputMetadata, QueryCodecError> {
        let projection = Self::newest_projection(envelope)?.ok_or(QueryCodecError::InvalidRow)?;
        let scope = decode_scope(&projection)?;
        let mut core_return_ns = None;
        let mut send_ns = None;
        for entry in Self::current_entries(envelope)? {
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
                    if metadata.query_id != scope.query_id {
                        return Err(QueryCodecError::InvalidRow);
                    }
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
                    return Ok(metadata);
                }
                _ => {}
            }
        }
        Err(QueryCodecError::InvalidRow)
    }

    pub fn append_post_commit_profiling(
        envelope: &mut ChangeEnvelope,
        component: &ComponentId,
        core_return_ns: u64,
        send_ns: u64,
    ) -> Result<(), QueryCodecError> {
        for (key, timestamp) in [
            (QUERY_CORE_RETURN_NS, core_return_ns),
            (QUERY_SEND_NS, send_ns),
        ] {
            envelope.append_annotation(ContextEntry::try_new(
                component.clone(),
                key,
                ContextValue::Unsigned(timestamp),
            )?)?;
        }
        Ok(())
    }

    pub fn without_post_commit_profiling(
        envelope: &ChangeEnvelope,
    ) -> Result<ChangeEnvelope, QueryCodecError> {
        let query = Self::metadata(envelope)?.query_id;
        let mut current = true;
        let mut entries: Vec<_> = envelope
            .annotations()
            .entries()
            .filter(|entry| {
                if entry.key() == QUERY_METADATA {
                    current = false;
                }
                !(current
                    && entry.contributor() == query
                    && matches!(entry.key(), QUERY_CORE_RETURN_NS | QUERY_SEND_NS))
            })
            .collect();
        entries.reverse();
        Ok(ChangeEnvelope::restore(
            envelope.id().clone(),
            envelope.changes().clone(),
            envelope.system().as_ref().clone(),
            envelope.lineage().cloned(),
            envelope.context_identity(),
            entries,
        )?)
    }

    /// Publish an evaluator result as ordinary graph changes. An evaluation with
    /// only `Noop` results publishes nothing.
    ///
    /// Records are full result nodes for `projector`'s output scope, with
    /// operation ordinals, full before images and aggregation flags preserved
    /// (flags only in the scoped projection context). Every image's effective
    /// time is the envelope event time (or its supplied production time when
    /// no event time exists), never an output sequence.
    pub fn encode_evaluation(
        input: Option<&ChangeEnvelope>,
        component: &ComponentId,
        system: SystemMetadata,
        projector: &ResultNodeProjector,
        results: &[QueryPartEvaluationContext],
        metadata: QueryOutputMetadata,
    ) -> Result<Option<ChangeEnvelope>, QueryCodecError> {
        let event_time = system.timestamp().unwrap_or(metadata.timestamp);
        let system = system
            .with_timestamp(event_time)
            .with_ordering(ChangeOrdering::ProducerSequence);
        let effective_from = u64::try_from(event_time.timestamp_millis())
            .map_err(|_| QueryCodecError::InvalidEventTime)?;
        let projection = projector.project_evaluation(results, effective_from)?;
        if projection.is_empty() {
            return Ok(None);
        }
        let (operations, operation_metadata) = projection.into_parts();
        let sequence = system.sequence();
        let id = super::emission_id(system.stream(), sequence)?;
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                system.stream().as_str(),
                Bytes::copy_from_slice(&sequence.to_be_bytes()),
            )?,
            GraphChangeCodec::schema().descriptor().clone(),
            operations,
        )?;
        let projection_entry = Self::projection_entry(
            component,
            changes.id(),
            &metadata.query_id,
            projector,
            &operation_metadata,
        )?;
        let mut envelope = match input {
            Some(input) => input.derive(id, changes, system),
            None => ChangeEnvelope::new(id, changes, system),
        };
        envelope.append_annotation(projection_entry)?;
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
        Ok(Some(envelope))
    }

    /// Reconstruct evaluator results from a current query output using its scoped
    /// projection context. This is a query-facing capability, not a wire format.
    pub fn decode_evaluation(
        envelope: &ChangeEnvelope,
    ) -> Result<Vec<QueryPartEvaluationContext>, QueryCodecError> {
        let projection = Self::projection(envelope)?;
        let projector = projection.projector();
        envelope
            .changes()
            .operations()
            .iter()
            .zip(projection.operations())
            .map(
                |(operation, meta)| -> Result<QueryPartEvaluationContext, QueryCodecError> {
                    let row_signature = Self::row_signature(&meta.row_identity)
                        .ok_or(QueryCodecError::InvalidRow)?;
                    let row = |record: &Record| -> Result<QueryVariables, QueryCodecError> {
                        let (identity, values) = projector.decode_row(record)?;
                        if identity != meta.row_identity {
                            return Err(QueryCodecError::InvalidRow);
                        }
                        Ok(values)
                    };
                    Ok(match operation {
                        ChangeOperation::Added { after, .. } => {
                            if meta.kind != QueryRowKind::Row {
                                return Err(QueryCodecError::InvalidRow);
                            }
                            QueryPartEvaluationContext::Adding {
                                after: row(after)?,
                                row_signature,
                            }
                        }
                        ChangeOperation::Updated {
                            before,
                            after,
                            semantics: UpdateSemantics::Replace,
                            ..
                        } => {
                            let after = row(after)?;
                            let before = before.as_ref().map(row).transpose()?;
                            match &meta.kind {
                                QueryRowKind::Row => QueryPartEvaluationContext::Updating {
                                    before: before.ok_or(QueryCodecError::MissingBefore)?,
                                    after,
                                    row_signature,
                                },
                                QueryRowKind::Aggregation {
                                    grouping_keys,
                                    default_before,
                                    default_after,
                                } => QueryPartEvaluationContext::Aggregation {
                                    before,
                                    after,
                                    grouping_keys: grouping_keys.clone(),
                                    default_before: *default_before,
                                    default_after: *default_after,
                                    row_signature,
                                },
                            }
                        }
                        ChangeOperation::Deleted {
                            identity, before, ..
                        } => {
                            if meta.kind != QueryRowKind::Row {
                                return Err(QueryCodecError::InvalidRow);
                            }
                            let before = before.as_ref().ok_or(QueryCodecError::MissingBefore)?;
                            if before.reference() != identity {
                                return Err(QueryCodecError::InvalidRow);
                            }
                            QueryPartEvaluationContext::Removing {
                                before: row(before)?,
                                row_signature,
                            }
                        }
                        _ => return Err(QueryCodecError::InvalidRow),
                    })
                },
            )
            .collect()
    }

    /// Project public result metadata without storing duplicate fields in native
    /// envelopes. Shared by ordinary APIs and native network serializers.
    pub fn result_metadata(
        envelope: &ChangeEnvelope,
    ) -> Result<QueryOutputMetadata, QueryCodecError> {
        let mut metadata = Self::metadata(envelope)?;
        if let Some(source) = &metadata.source_id {
            metadata
                .metadata
                .entry("source_id".into())
                .or_insert_with(|| serde_json::json!(source));
        }
        metadata
            .metadata
            .entry("processed_by".into())
            .or_insert_with(|| serde_json::json!("drasi-core"));
        metadata
            .metadata
            .entry("result_count".into())
            .or_insert_with(|| serde_json::json!(envelope.changes().operations().len()));
        Ok(metadata)
    }

    /// Project native results into the ordinary query-result boundary. This is a
    /// query-facing JSON capability, not a default wire representation.
    pub fn to_legacy_result(envelope: &ChangeEnvelope) -> Result<QueryResult, QueryCodecError> {
        let metadata = Self::result_metadata(envelope)?;
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

#[cfg(test)]
mod test_support {
    use super::*;
    use crate::StreamId;
    use drasi_core_types::evaluation::variable_value::VariableValue;

    pub(super) fn projector(dataset: &str) -> ResultNodeProjector {
        ResultNodeProjector::try_new(dataset, Some("Comfort")).unwrap()
    }

    pub(super) fn vars(name: &str) -> QueryVariables {
        QueryVariables::from([
            ("name".into(), VariableValue::from(name)),
            ("room".into(), VariableValue::from("r1")),
        ])
    }

    pub(super) fn system(stream: &str, sequence: u64) -> SystemMetadata {
        SystemMetadata::new(StreamId::try_new(stream).unwrap(), sequence)
    }

    pub(super) fn metadata(query: &str) -> QueryOutputMetadata {
        QueryOutputMetadata {
            query_id: query.into(),
            source_id: Some("source".into()),
            timestamp: DateTime::from_timestamp_millis(1_000).unwrap(),
            metadata: HashMap::new(),
            profiling: None,
        }
    }

    pub(super) fn adding(name: &str, row_signature: u64) -> QueryPartEvaluationContext {
        QueryPartEvaluationContext::Adding {
            after: vars(name),
            row_signature,
        }
    }

    pub(super) fn encode(
        input: Option<&ChangeEnvelope>,
        query: &str,
        projector: &ResultNodeProjector,
        results: &[QueryPartEvaluationContext],
    ) -> ChangeEnvelope {
        QueryChangeCodec::encode_evaluation(
            input,
            &ComponentId::try_new(query).unwrap(),
            system(&format!("{query}/out"), 1),
            projector,
            results,
            metadata(query),
        )
        .unwrap()
        .unwrap()
    }
}

#[cfg(test)]
mod recovery_envelope_tests {
    use super::test_support::*;
    use super::*;
    use crate::{EnvelopeCodec, QueryResultSnapshot};
    use std::num::NonZeroUsize;

    fn rows(projector: &ResultNodeProjector, signatures: &[u64]) -> im::HashMap<RecordId, Record> {
        let mut rows = im::HashMap::new();
        for signature in signatures {
            let record =
                QueryChangeCodec::encode_row(projector, *signature, &vars("row"), 1_000).unwrap();
            rows.insert(record.identity().clone(), record);
        }
        rows
    }

    #[test]
    fn progress_envelope_is_distinct_from_snapshot_and_preserves_query_position() {
        let component = ComponentId::try_new("replay").unwrap();
        let scope = projector("scope/replay");
        let envelope = QueryChangeCodec::progress_envelope(
            &scope,
            "query",
            42,
            9,
            &component,
            system("replay/out", 7),
        )
        .unwrap();
        assert!(QueryChangeCodec::is_query_output(&envelope));
        assert!(QueryChangeCodec::is_progress_only(&envelope));
        assert!(!QueryChangeCodec::is_snapshot(&envelope));
        assert!(envelope.changes().is_empty());
        assert_eq!(
            envelope.changes().schema(),
            GraphChangeCodec::schema().descriptor()
        );
        assert_eq!(
            QueryChangeCodec::projection(&envelope).unwrap().projector(),
            &scope
        );
        assert_eq!(envelope.system().sequence(), 7);
        assert_eq!(QueryChangeCodec::query_sequence(&envelope).unwrap(), 42);
        assert_eq!(QueryChangeCodec::query_generation(&envelope).unwrap(), 9);
        assert_eq!(
            QueryChangeCodec::metadata(&envelope).unwrap().query_id,
            "query"
        );
        assert!(QueryChangeCodec::to_legacy_result(&envelope).is_err());

        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1024 * 1024).unwrap());
        codec.register_schema(QueryChangeCodec::schema()).unwrap();
        let decoded = codec.decode(&codec.encode(&envelope).unwrap()).unwrap();
        assert!(QueryChangeCodec::is_query_output(&decoded));
        assert!(QueryChangeCodec::is_progress_only(&decoded));
        assert!(!QueryChangeCodec::is_snapshot(&decoded));
        assert!(decoded.changes().is_empty());
        assert_eq!(QueryChangeCodec::query_sequence(&decoded).unwrap(), 42);
        assert_eq!(QueryChangeCodec::query_generation(&decoded).unwrap(), 9);

        let snapshot = QueryChangeCodec::snapshot_envelope(
            &scope,
            "query",
            &QueryResultSnapshot {
                projector: scope.clone(),
                rows: im::HashMap::new(),
                as_of_sequence: 42,
                generation: 9,
            },
            &component,
            system("replay/out", 8),
        )
        .unwrap();
        assert!(QueryChangeCodec::is_snapshot(&snapshot));
        assert!(!QueryChangeCodec::is_progress_only(&snapshot));
        assert_eq!(QueryChangeCodec::query_sequence(&snapshot).unwrap(), 42);
        assert_eq!(QueryChangeCodec::query_generation(&snapshot).unwrap(), 9);
    }

    #[test]
    fn published_query_envelopes_use_producer_sequence_and_keep_business_time() {
        let component = ComponentId::try_new("replay").unwrap();
        let scope = projector("scope/replay");
        let business_time = DateTime::from_timestamp_millis(2_000).unwrap();
        let supplied = |sequence| {
            system("replay/out", sequence)
                .with_timestamp(business_time)
                .with_ordering(ChangeOrdering::EventTime)
        };

        let evaluation = QueryChangeCodec::encode_evaluation(
            None,
            &component,
            supplied(1),
            &scope,
            &[adding("a", 1)],
            metadata("replay"),
        )
        .unwrap()
        .unwrap();
        let progress =
            QueryChangeCodec::progress_envelope(&scope, "replay", 2, 1, &component, supplied(2))
                .unwrap();
        let snapshot = QueryChangeCodec::snapshot_envelope(
            &scope,
            "replay",
            &QueryResultSnapshot {
                projector: scope.clone(),
                rows: im::HashMap::new(),
                as_of_sequence: 2,
                generation: 1,
            },
            &component,
            supplied(3),
        )
        .unwrap();
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1024 * 1024).unwrap());
        codec.register_schema(QueryChangeCodec::schema()).unwrap();
        for envelope in [evaluation, progress, snapshot] {
            assert_eq!(
                envelope.system().ordering(),
                ChangeOrdering::ProducerSequence
            );
            assert_eq!(envelope.system().timestamp(), Some(business_time));
            let decoded = codec.decode(&codec.encode(&envelope).unwrap()).unwrap();
            assert_eq!(
                decoded.system().ordering(),
                ChangeOrdering::ProducerSequence
            );
            assert_eq!(decoded.system().timestamp(), Some(business_time));
        }

        let stamped = encode(None, "fallback", &scope, &[adding("a", 1)]);
        assert_eq!(
            stamped.system().ordering(),
            ChangeOrdering::ProducerSequence
        );
        assert_eq!(
            stamped.system().timestamp(),
            Some(DateTime::from_timestamp_millis(1_000).unwrap())
        );
    }

    #[test]
    fn empty_snapshot_keeps_its_explicit_scope_and_rows_use_common_graph_records() {
        let component = ComponentId::try_new("replay").unwrap();
        let scope = projector("scope/a");
        let empty = QueryChangeCodec::snapshot_envelope(
            &scope,
            "query",
            &QueryResultSnapshot {
                projector: scope.clone(),
                rows: im::HashMap::new(),
                as_of_sequence: 3,
                generation: 1,
            },
            &component,
            system("replay/out", 1),
        )
        .unwrap();
        assert!(empty.changes().is_empty());
        assert!(QueryChangeCodec::is_query_output(&empty));
        assert_eq!(
            QueryChangeCodec::projection(&empty).unwrap().projector(),
            &scope
        );

        let snapshot = QueryResultSnapshot {
            projector: scope.clone(),
            rows: rows(&scope, &[9, 2, 5]),
            as_of_sequence: 11,
            generation: 4,
        };
        let envelope = QueryChangeCodec::snapshot_envelope(
            &scope,
            "query",
            &snapshot,
            &component,
            system("replay/out", 2),
        )
        .unwrap();
        assert!(QueryChangeCodec::is_snapshot(&envelope));
        assert_eq!(QueryChangeCodec::query_sequence(&envelope).unwrap(), 11);
        let operations = envelope.changes().operations();
        assert_eq!(
            operations
                .iter()
                .map(ChangeOperation::ordinal)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(operations
            .iter()
            .all(|operation| matches!(operation, ChangeOperation::Added { .. })));
        let projection = QueryChangeCodec::projection(&envelope).unwrap();
        assert!(projection
            .operations()
            .iter()
            .all(|meta| meta.kind == QueryRowKind::Row));
        let mut signatures: Vec<_> = projection
            .operations()
            .iter()
            .map(|meta| QueryChangeCodec::row_signature(&meta.row_identity).unwrap())
            .collect();
        signatures.sort_unstable();
        assert_eq!(signatures, [2, 5, 9]);
        assert_eq!(
            GraphChangeCodec::decode_changes(&envelope).unwrap().len(),
            3
        );
    }

    #[test]
    fn snapshot_rejects_rows_outside_the_explicit_scope_or_under_the_wrong_key() {
        let component = ComponentId::try_new("replay").unwrap();
        let scope = projector("scope/a");
        let other = projector("scope/b");
        let build = |rows| {
            QueryChangeCodec::snapshot_envelope(
                &scope,
                "query",
                &QueryResultSnapshot {
                    projector: scope.clone(),
                    rows,
                    as_of_sequence: 1,
                    generation: 1,
                },
                &component,
                system("replay/out", 1),
            )
        };
        assert!(build(rows(&scope, &[1])).is_ok());
        assert!(build(rows(&other, &[1])).is_err());

        let first = QueryChangeCodec::encode_row(&scope, 1, &vars("a"), 1).unwrap();
        let second = QueryChangeCodec::encode_row(&scope, 2, &vars("b"), 1).unwrap();
        let mut mismatched = im::HashMap::new();
        mismatched.insert(second.identity().clone(), first);
        assert!(build(mismatched).is_err());

        let relabeled = ResultNodeProjector::try_new("scope/a", Some("Other")).unwrap();
        assert!(build(rows(&relabeled, &[1])).is_err());
    }

    #[test]
    fn legacy_snapshot_rows_validate_scope_and_signature_before_projection() {
        let scope = projector("scope/a");
        let before = QueryChangeCodec::snapshot_row_projections();
        let projected: Vec<_> =
            QueryChangeCodec::legacy_snapshot_rows(&scope, rows(&scope, &[3, 4]))
                .unwrap()
                .collect();
        assert_eq!(projected.len(), 2);
        assert_eq!(QueryChangeCodec::snapshot_row_projections() - before, 2);
        let mut signatures: Vec<_> = projected.iter().map(|(signature, _)| *signature).collect();
        signatures.sort_unstable();
        assert_eq!(signatures, [3, 4]);
        assert_eq!(projected[0].1["name"], serde_json::json!("row"));

        assert!(
            QueryChangeCodec::legacy_snapshot_rows(&projector("scope/b"), rows(&scope, &[3]))
                .is_err()
        );
        let unsigned = scope.record(&[1, 2, 3], &vars("x"), 1).unwrap();
        let mut by_key = im::HashMap::new();
        by_key.insert(unsigned.identity().clone(), unsigned);
        assert!(QueryChangeCodec::legacy_snapshot_rows(&scope, by_key).is_err());
    }
}

#[cfg(test)]
mod projection_tests {
    use super::test_support::*;
    use super::*;
    use crate::{
        ElementBeforeImage, RecordImage, RecordValidationError, RecordValidator, SchemaDescriptor,
        SchemaId, SchemaVersion, StreamId,
    };
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };

    fn source_insert() -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("db", "n1"),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 5,
                },
                properties: ElementPropertyMap::new(),
            },
        }
    }

    fn component(name: &str) -> ComponentId {
        ComponentId::try_new(name).unwrap()
    }

    #[test]
    fn appended_metadata_cannot_relabel_the_query_that_owns_the_projection() {
        let scope = projector("scope/a");
        let mut output = encode(None, "query", &scope, &[adding("a", 1)]);
        output
            .append_annotation(
                ContextEntry::try_new(
                    component("foreign"),
                    QUERY_METADATA,
                    ContextValue::Bytes(Arc::from(
                        serde_json::to_vec(&metadata("another-query")).unwrap(),
                    )),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            QueryChangeCodec::metadata(&output),
            Err(QueryCodecError::InvalidRow)
        ));
        assert!(QueryChangeCodec::to_legacy_result(&output).is_err());
        assert!(GraphChangeCodec::decode_changes(&output).is_ok());
    }

    #[test]
    fn source_and_query_records_share_the_exact_schema_and_generic_decoder() {
        assert!(Arc::ptr_eq(
            &QueryChangeCodec::schema(),
            &GraphChangeCodec::schema()
        ));
        let scope = projector("scope/a");
        let query = encode(None, "query", &scope, &[adding("a", 1)]);
        let source = GraphChangeCodec::encode_change(
            source_insert(),
            StreamId::try_new("db/out").unwrap(),
            1,
            None,
        )
        .unwrap();
        assert_eq!(query.changes().schema(), source.changes().schema());
        assert_eq!(
            query.changes().schema(),
            GraphChangeCodec::schema().descriptor()
        );

        let decoded = GraphChangeCodec::decode_changes(&query).unwrap();
        let [SourceChange::Insert {
            element: Element::Node { metadata, .. },
        }] = decoded.as_slice()
        else {
            panic!("a query row is one ordinary graph node insert");
        };
        assert_eq!(metadata.labels.as_ref(), [Arc::from("Comfort")]);
        assert_eq!(metadata.reference.source_id.as_ref(), "scope/a");
        assert!(GraphChangeCodec::decode_changes(&source).is_ok());
    }

    #[test]
    fn node_properties_contain_only_the_returned_aliases() {
        let scope = projector("scope/a");
        let envelope = encode(None, "query", &scope, &[adding("a", 7)]);
        let ChangeOperation::Added { after, .. } = &envelope.changes().operations()[0] else {
            panic!("expected an add");
        };
        let image = GraphChangeCodec::decode_element(after).unwrap();
        assert_eq!(
            image
                .properties()
                .keys()
                .map(|key| key.as_str())
                .collect::<Vec<_>>(),
            ["name", "room"]
        );
        assert_eq!(image.metadata().effective_from, 1_000);
        let row = QueryChangeCodec::decode_row(after).unwrap();
        assert_eq!(row.dataset, "scope/a");
        assert_eq!(row.label, "Comfort");
        assert_eq!(row.signature, Some(7));
        assert_eq!(row.identity, 7u64.to_be_bytes()[..]);
        assert_eq!(row.values, vars("a"));
    }

    #[test]
    fn scopes_distinguish_outputs_and_same_values_keep_distinct_row_identities() {
        let first = projector("instance-a/out");
        let second = projector("instance-b/out");
        let a = encode(
            None,
            "query",
            &first,
            &[adding("same", 1), adding("same", 2)],
        );
        let b = encode(None, "query", &second, &[adding("same", 1)]);
        let identities: Vec<_> = a
            .changes()
            .operations()
            .iter()
            .chain(b.changes().operations())
            .map(|operation| operation.identity().clone())
            .collect();
        assert_eq!(
            identities
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3
        );
        let ChangeOperation::Added { after, .. } = &a.changes().operations()[1] else {
            panic!("expected an add");
        };
        assert_eq!(
            QueryChangeCodec::decode_row(after).unwrap().values,
            vars("same")
        );
        assert_eq!(
            QueryChangeCodec::decode_row_in(&first, after)
                .unwrap()
                .signature,
            Some(2)
        );
        assert!(QueryChangeCodec::decode_row_in(&second, after).is_err());
        assert_ne!(
            QueryChangeCodec::projection(&a).unwrap().projector(),
            QueryChangeCodec::projection(&b).unwrap().projector()
        );
    }

    #[test]
    fn evaluation_round_trips_with_aggregate_metadata_kept_outside_node_data() {
        let scope = projector("scope/a");
        let results = vec![
            QueryPartEvaluationContext::Noop,
            adding("a", 1),
            QueryPartEvaluationContext::Updating {
                before: vars("b"),
                after: vars("c"),
                row_signature: 2,
            },
            QueryPartEvaluationContext::Aggregation {
                before: None,
                after: vars("g"),
                grouping_keys: vec!["room".into()],
                default_before: true,
                default_after: false,
                row_signature: 3,
            },
            QueryPartEvaluationContext::Removing {
                before: vars("d"),
                row_signature: 4,
            },
        ];
        let envelope = encode(None, "query", &scope, &results);
        assert_eq!(
            envelope
                .changes()
                .operations()
                .iter()
                .map(ChangeOperation::ordinal)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert_eq!(
            QueryChangeCodec::decode_evaluation(&envelope).unwrap(),
            results[1..]
        );

        let projection = QueryChangeCodec::projection(&envelope).unwrap();
        assert_eq!(projection.operations()[0].kind, QueryRowKind::Row);
        assert_eq!(
            projection.operations()[2].kind,
            QueryRowKind::Aggregation {
                grouping_keys: vec!["room".into()],
                default_before: true,
                default_after: false,
            }
        );
        let ChangeOperation::Updated { before, after, .. } = &envelope.changes().operations()[2]
        else {
            panic!("an aggregation replaces one row");
        };
        assert!(before.is_none());
        let image = GraphChangeCodec::decode_element(after).unwrap();
        assert_eq!(
            image
                .properties()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["name", "room"]
        );
        assert_eq!(
            GraphChangeCodec::decode_changes(&envelope).unwrap().len(),
            4
        );
        let ChangeOperation::Updated {
            before: Some(before),
            ..
        } = &envelope.changes().operations()[1]
        else {
            panic!("an update has a full before image");
        };
        assert_eq!(before.image(), RecordImage::Full);

        let legacy = QueryChangeCodec::to_legacy_result(&envelope).unwrap();
        assert_eq!(legacy.query_id, "query");
        assert_eq!(legacy.results.len(), 4);
    }

    #[test]
    fn all_noop_evaluation_publishes_nothing_and_negative_event_time_is_rejected() {
        let scope = projector("scope/a");
        assert!(QueryChangeCodec::encode_evaluation(
            None,
            &component("query"),
            system("query/out", 1),
            &scope,
            &[QueryPartEvaluationContext::Noop],
            metadata("query"),
        )
        .unwrap()
        .is_none());
        let mut early = metadata("query");
        early.timestamp = DateTime::from_timestamp_millis(-1).unwrap();
        assert!(matches!(
            QueryChangeCodec::encode_evaluation(
                None,
                &component("query"),
                system("query/out", 1),
                &scope,
                &[adding("a", 1)],
                early,
            ),
            Err(QueryCodecError::InvalidEventTime)
        ));
    }

    #[test]
    fn projection_context_is_not_reread_by_a_fresh_derive_but_survives_reemit() {
        let scope = projector("scope/up");
        let mut upstream = encode(None, "upstream", &scope, &[adding("a", 1)]);
        QueryChangeCodec::set_generation(&mut upstream, &component("upstream"), 5).unwrap();
        assert!(QueryChangeCodec::is_query_output(&upstream));
        assert_eq!(QueryChangeCodec::query_generation(&upstream).unwrap(), 5);

        let again = upstream.reemit(9).unwrap();
        assert_eq!(again.system().sequence(), 9);
        assert!(QueryChangeCodec::is_query_output(&again));
        assert_eq!(
            QueryChangeCodec::projection(&again).unwrap(),
            QueryChangeCodec::projection(&upstream).unwrap()
        );
        assert_eq!(QueryChangeCodec::query_sequence(&again).unwrap(), 1);
        assert_eq!(QueryChangeCodec::output_sequence(&again).unwrap(), 1);
        assert_eq!(QueryChangeCodec::query_generation(&again).unwrap(), 5);
        assert_eq!(
            QueryChangeCodec::metadata(&again).unwrap().query_id,
            "upstream"
        );

        let derived = GraphChangeCodec::derive_changes(
            &upstream,
            &[source_insert()],
            StreamId::try_new("derived/out").unwrap(),
            1,
        )
        .unwrap();
        assert!(!QueryChangeCodec::is_query_output(&derived));
        assert!(!QueryChangeCodec::is_snapshot(&derived));
        assert!(!QueryChangeCodec::is_progress_only(&derived));
        assert!(QueryChangeCodec::projection(&derived).is_err());
        assert!(QueryChangeCodec::metadata(&derived).is_err());
        assert!(QueryChangeCodec::query_generation(&derived).is_err());
        assert!(QueryChangeCodec::query_sequence(&derived).is_err());
        assert!(QueryChangeCodec::decode_evaluation(&derived).is_err());
        assert!(GraphChangeCodec::decode_changes(&derived).is_ok());

        let downstream = encode(
            Some(&upstream),
            "downstream",
            &projector("scope/down"),
            &[adding("b", 1)],
        );
        assert_eq!(
            QueryChangeCodec::metadata(&downstream).unwrap().query_id,
            "downstream"
        );
        assert_eq!(
            QueryChangeCodec::query_generation(&downstream).unwrap(),
            0,
            "an upstream generation is inherited history, not this output's"
        );
    }

    #[test]
    fn malformed_unscoped_or_misaligned_projection_context_is_rejected() {
        let scope = projector("scope/a");
        let envelope = encode(None, "query", &scope, &[adding("a", 1), adding("b", 2)]);
        let valid = QueryChangeCodec::projection(&envelope).unwrap();
        let tamper = |operations: Vec<ResultOperationMetadata>| {
            let mut branch = envelope.clone();
            branch
                .append_annotation(
                    QueryChangeCodec::projection_entry(
                        &component("query"),
                        envelope.changes().id(),
                        "query",
                        &scope,
                        &operations,
                    )
                    .unwrap(),
                )
                .unwrap();
            branch
        };
        let with = |index: usize, edit: &dyn Fn(&mut ResultOperationMetadata)| {
            let mut operations = valid.operations().to_vec();
            edit(&mut operations[index]);
            tamper(operations)
        };
        let wrong_identity = with(1, &|meta| meta.row_identity = Bytes::from_static(&[9; 8]));
        let wrong_ordinal = with(1, &|meta| meta.ordinal = 7);
        let missing = tamper(valid.operations()[..1].to_vec());
        for branch in [&wrong_identity, &wrong_ordinal, &missing] {
            assert!(!QueryChangeCodec::is_query_output(branch));
            assert!(QueryChangeCodec::decode_evaluation(branch).is_err());
            assert!(QueryChangeCodec::to_legacy_result(branch).is_err());
        }
        let aggregate_add = with(0, &|meta| {
            meta.kind = QueryRowKind::Aggregation {
                grouping_keys: Vec::new(),
                default_before: false,
                default_after: false,
            }
        });
        assert!(QueryChangeCodec::is_query_output(&aggregate_add));
        assert!(QueryChangeCodec::decode_evaluation(&aggregate_add).is_err());

        for value in [
            ContextValue::Bytes(Arc::from(&b"garbage"[..])),
            ContextValue::Bytes(Arc::from(&[][..])),
            ContextValue::Bool(true),
        ] {
            let mut branch = envelope.clone();
            branch
                .append_annotation(
                    ContextEntry::try_new(component("query"), QUERY_PROJECTION, value).unwrap(),
                )
                .unwrap();
            assert!(!QueryChangeCodec::is_query_output(&branch));
            assert!(QueryChangeCodec::metadata(&branch).is_err());
            assert!(QueryChangeCodec::query_sequence(&branch).is_err());
        }

        let future_version = ProjectionContext {
            scope: ProjectionScope {
                version: 2,
                change_set_namespace: envelope.changes().id().namespace().to_owned(),
                change_set_value: envelope.changes().id().value().to_vec(),
                query_id: "query".into(),
            },
            dataset: "scope/a".into(),
            label: "Comfort".into(),
            operations: Vec::new(),
        };
        let mut branch = envelope.clone();
        branch
            .append_annotation(
                ContextEntry::try_new(
                    component("query"),
                    QUERY_PROJECTION,
                    ContextValue::Bytes(Arc::from(encode_context(&future_version).unwrap())),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(!QueryChangeCodec::is_query_output(&branch));
        assert!(QueryChangeCodec::metadata(&branch).is_err());
    }

    #[test]
    fn plain_graph_changes_and_old_query_row_data_are_not_query_output() {
        let plain = GraphChangeCodec::encode_change(
            source_insert(),
            StreamId::try_new("db/out").unwrap(),
            1,
            None,
        )
        .unwrap();
        assert!(!QueryChangeCodec::is_query_output(&plain));
        assert!(QueryChangeCodec::metadata(&plain).is_err());
        assert!(QueryChangeCodec::query_sequence(&plain).is_err());
        assert!(QueryChangeCodec::decode_evaluation(&plain).is_err());
        assert!(QueryChangeCodec::to_legacy_result(&plain).is_err());

        struct Old;
        impl RecordValidator for Old {
            fn validate_identity(
                &self,
                _: &SchemaDescriptor,
                _: &RecordId,
            ) -> Result<(), RecordValidationError> {
                Ok(())
            }
            fn validate(
                &self,
                _: &SchemaDescriptor,
                _: &RecordId,
                _: RecordImage,
                _: &[u8],
            ) -> Result<(), RecordValidationError> {
                Ok(())
            }
        }
        let old_schema = Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("drasi.query-row").unwrap(),
                SchemaVersion::try_new(1).unwrap(),
                "drasi-query-row-bincode1",
                Bytes::from_static(b"query-row-v1"),
            )
            .unwrap(),
            Arc::new(Old),
        );
        let old = Record::try_new(
            &old_schema,
            RecordId::try_new("drasi.query-row/7175657279", Bytes::from_static(&[0; 8])).unwrap(),
            RecordImage::Full,
            Bytes::from_static(b"serialized-old-row"),
        )
        .unwrap();
        assert!(QueryChangeCodec::decode_row(&old).is_err());
        assert!(QueryChangeCodec::decode_row_in(&projector("scope/a"), &old).is_err());

        let stream = StreamId::try_new("old/out").unwrap();
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new("old/out", Bytes::from_static(&[1])).unwrap(),
            old_schema.descriptor().clone(),
            vec![ChangeOperation::Added {
                ordinal: 0,
                after: old,
            }],
        )
        .unwrap();
        let envelope = ChangeEnvelope::new(
            crate::emission_id(&stream, 1).unwrap(),
            changes,
            SystemMetadata::new(stream, 1),
        );
        assert!(!QueryChangeCodec::is_query_output(&envelope));
        assert!(QueryChangeCodec::decode_evaluation(&envelope).is_err());
    }

    #[test]
    fn row_decoding_requires_full_result_nodes_of_the_expected_scope() {
        let scope = projector("scope/a");
        let record = QueryChangeCodec::encode_row(&scope, 1, &vars("a"), 1).unwrap();
        assert!(QueryChangeCodec::decode_row_in(&scope, &record).is_ok());
        let relabeled = ResultNodeProjector::try_new("scope/a", Some("Other")).unwrap();
        assert!(QueryChangeCodec::decode_row_in(&relabeled, &record).is_err());

        let partial =
            GraphChangeCodec::partial_record(&ElementBeforeImage::Metadata(ElementMetadata {
                reference: scope.reference(&1u64.to_be_bytes()).unwrap(),
                labels: Arc::from([Arc::from("Comfort")]),
                effective_from: 1,
            }))
            .unwrap();
        assert!(QueryChangeCodec::decode_row(&partial).is_err());
        assert!(QueryChangeCodec::decode_row_in(&scope, &partial).is_err());
    }

    #[test]
    fn update_without_a_before_image_is_rejected_when_it_is_not_an_aggregation() {
        let scope = projector("scope/a");
        let envelope = encode(
            None,
            "query",
            &scope,
            &[QueryPartEvaluationContext::Aggregation {
                before: None,
                after: vars("g"),
                grouping_keys: Vec::new(),
                default_before: false,
                default_after: false,
                row_signature: 1,
            }],
        );
        assert!(QueryChangeCodec::decode_evaluation(&envelope).is_ok());
        let mut branch = envelope.clone();
        let operations = QueryChangeCodec::projection(&envelope)
            .unwrap()
            .operations()
            .iter()
            .cloned()
            .map(|mut meta| {
                meta.kind = QueryRowKind::Row;
                meta
            })
            .collect::<Vec<_>>();
        branch
            .append_annotation(
                QueryChangeCodec::projection_entry(
                    &component("query"),
                    envelope.changes().id(),
                    "query",
                    &scope,
                    &operations,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            QueryChangeCodec::decode_evaluation(&branch),
            Err(QueryCodecError::MissingBefore)
        ));
    }
}

#[cfg(test)]
mod profiling_tests {
    use super::test_support::*;
    use super::*;
    use drasi_core_types::evaluation::variable_value::VariableValue;
    use std::{collections::BTreeMap, num::NonZeroUsize};

    fn output(
        input: Option<&ChangeEnvelope>,
        query: &str,
        profiling: Option<ProfilingMetadata>,
    ) -> ChangeEnvelope {
        let mut query_metadata = metadata(query);
        query_metadata.metadata = HashMap::from([("preserved".into(), serde_json::json!(true))]);
        query_metadata.profiling = profiling;
        QueryChangeCodec::encode_evaluation(
            input,
            &ComponentId::try_new(query).unwrap(),
            system(&format!("{query}/out"), 1),
            &projector(&format!("{query}/dataset")),
            &[QueryPartEvaluationContext::Adding {
                after: BTreeMap::from([("value".into(), VariableValue::from(1))]),
                row_signature: 7,
            }],
            query_metadata,
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
    fn graph_producer_query_receipts_ignore_only_current_post_commit_timings() {
        use crate::output_identity::{Candidate, Producer};
        use crate::{EnvelopeCodec, GraphProducerIdentity, GraphProducerProgress};

        let query = ComponentId::try_new("query").unwrap();
        let mut stored = output(None, query.as_str(), None);
        let identity = GraphProducerIdentity::new(
            "instance".into(),
            "graph".into(),
            query.clone(),
            stored.system().stream().clone(),
            true,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut stored, &identity, 1).unwrap();
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024).unwrap());
        codec.register_schema(GraphChangeCodec::schema()).unwrap();
        let saved = Candidate::new(&stored, &codec).unwrap();
        assert!(matches!(saved.producer, Producer::Graph(_)));
        let mut live = stored.reemit(17).unwrap();
        QueryChangeCodec::append_post_commit_profiling(&mut live, &query, 30, 40).unwrap();
        let retry = Candidate::new(&live, &codec).unwrap();
        assert_eq!(saved.producer, retry.producer);
        assert_eq!(saved.sequence, retry.sequence);
        assert_eq!(saved.digest, retry.digest);

        let downstream = ComponentId::try_new("downstream").unwrap();
        let mut derived = output(Some(&live), downstream.as_str(), None);
        let identity = GraphProducerIdentity::new(
            "instance".into(),
            "graph".into(),
            downstream.clone(),
            derived.system().stream().clone(),
            true,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut derived, &identity, 1).unwrap();
        let saved = Candidate::new(&derived, &codec).unwrap();
        QueryChangeCodec::append_post_commit_profiling(&mut derived, &downstream, 50, 60).unwrap();
        assert_eq!(
            saved.digest,
            Candidate::new(&derived, &codec).unwrap().digest
        );
        QueryChangeCodec::append_post_commit_profiling(&mut derived, &query, 70, 80).unwrap();
        assert_ne!(
            saved.digest,
            Candidate::new(&derived, &codec).unwrap().digest,
            "another producer's timings remain part of the retained content",
        );
    }

    #[test]
    fn public_result_metadata_is_projected_without_rewriting_native_metadata() {
        let envelope = output(None, "query", None);
        let original = metadata_bytes(&envelope);
        let expected = HashMap::from([
            ("preserved".into(), serde_json::json!(true)),
            ("source_id".into(), serde_json::json!("source")),
            ("processed_by".into(), serde_json::json!("drasi-core")),
            ("result_count".into(), serde_json::json!(1)),
        ]);
        assert_eq!(
            QueryChangeCodec::result_metadata(&envelope)
                .unwrap()
                .metadata,
            expected
        );
        assert_eq!(
            QueryChangeCodec::to_legacy_result(&envelope)
                .unwrap()
                .metadata,
            expected
        );
        assert_eq!(
            QueryChangeCodec::metadata(&envelope).unwrap().metadata,
            HashMap::from([("preserved".into(), serde_json::json!(true))])
        );
        assert_eq!(metadata_bytes(&envelope), original);
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
        assert!(QueryChangeCodec::is_query_output(&decoded));
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
    fn embedded_profiling_remains_in_current_metadata_without_new_annotations() {
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
