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

//! Internal immutable change contracts.
//!
//! These types intentionally remain crate-private. The compatibility adapters are
//! not connected to the live pipeline until the next migration layer.

mod adapters;

pub(crate) use adapters::{
    query_evaluation_to_envelope, query_result_from_envelope, source_event_from_envelope,
    source_event_to_envelope, ChangeAdapterError, QueryEnvelopeMetadata,
};

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, OnceLock},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core::{
    evaluation::context::QueryVariables,
    interface::FutureElementRef,
    models::{Element, ElementMetadata, ElementReference},
};

use crate::profiling::ProfilingMetadata;

pub(crate) type ChangeSetRef = Arc<ChangeSet>;
pub(crate) type SchemaRef = Arc<ChangeSchema>;

const GRAPH_SCHEMA_DESCRIPTOR: &str =
    "drasi.internal.graph-change/v1;element|metadata|future;add|patch|delete";
const QUERY_SCHEMA_DESCRIPTOR: &str =
    "drasi.internal.query-result/v1;query-variables;add|replace|delete|aggregation";

const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

const GRAPH_SCHEMA_FINGERPRINT: u64 = fnv1a(GRAPH_SCHEMA_DESCRIPTOR.as_bytes());
const QUERY_SCHEMA_FINGERPRINT: u64 = fnv1a(QUERY_SCHEMA_DESCRIPTOR.as_bytes());

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ChangeSchemaKind {
    GraphChange,
    QueryResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SchemaVersion(u32);

impl SchemaVersion {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SchemaFingerprint(u64);

impl SchemaFingerprint {
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ChangeSchema {
    id: &'static str,
    version: SchemaVersion,
    fingerprint: SchemaFingerprint,
    kind: ChangeSchemaKind,
}

impl ChangeSchema {
    pub(crate) fn id(&self) -> &'static str {
        self.id
    }

    pub(crate) const fn version(&self) -> SchemaVersion {
        self.version
    }

    pub(crate) const fn fingerprint(&self) -> SchemaFingerprint {
        self.fingerprint
    }

    pub(crate) const fn kind(&self) -> ChangeSchemaKind {
        self.kind
    }
}

pub(crate) fn graph_change_schema() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            Arc::new(ChangeSchema {
                id: "drasi.internal.graph-change",
                version: SchemaVersion::new(1),
                fingerprint: SchemaFingerprint(GRAPH_SCHEMA_FINGERPRINT),
                kind: ChangeSchemaKind::GraphChange,
            })
        })
        .clone()
}

pub(crate) fn query_result_schema() -> SchemaRef {
    static SCHEMA: OnceLock<SchemaRef> = OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            Arc::new(ChangeSchema {
                id: "drasi.internal.query-result",
                version: SchemaVersion::new(1),
                fingerprint: SchemaFingerprint(QUERY_SCHEMA_FINGERPRINT),
                kind: ChangeSchemaKind::QueryResult,
            })
        })
        .clone()
}

macro_rules! identity_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub(crate) struct $name(u64);

        impl $name {
            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }

            pub(crate) const fn value(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{:016x}", self.0)
            }
        }
    };
}

identity_type!(ChangeSetId);
identity_type!(EnvelopeId);
identity_type!(ContextRootId);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RecordIdentity {
    GraphElement(ElementReference),
    QueryRow(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GraphRecord {
    Element(Arc<Element>),
    Metadata(Arc<ElementMetadata>),
    Future(Arc<FutureElementRef>),
}

impl GraphRecord {
    fn reference(&self) -> &ElementReference {
        match self {
            Self::Element(element) => element.get_reference(),
            Self::Metadata(metadata) => &metadata.reference,
            Self::Future(future) => &future.element_ref,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RecordData {
    Graph(GraphRecord),
    QueryRow(Arc<QueryVariables>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum UpdateSemantics {
    Patch,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum UpdateMetadata {
    None,
    QueryUpdate {
        grouping_keys: Option<Arc<[Arc<str>]>>,
    },
    QueryAggregation {
        grouping_keys: Arc<[Arc<str>]>,
        default_before: bool,
        default_after: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AddedRecord {
    ordinal: usize,
    identity: RecordIdentity,
    after: RecordData,
}

impl AddedRecord {
    pub(crate) fn new(ordinal: usize, identity: RecordIdentity, after: RecordData) -> Self {
        Self {
            ordinal,
            identity,
            after,
        }
    }

    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub(crate) fn identity(&self) -> &RecordIdentity {
        &self.identity
    }

    pub(crate) fn after(&self) -> &RecordData {
        &self.after
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UpdatedRecord {
    ordinal: usize,
    identity: RecordIdentity,
    before: Option<RecordData>,
    after: RecordData,
    semantics: UpdateSemantics,
    metadata: UpdateMetadata,
}

impl UpdatedRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ordinal: usize,
        identity: RecordIdentity,
        before: Option<RecordData>,
        after: RecordData,
        semantics: UpdateSemantics,
        metadata: UpdateMetadata,
    ) -> Self {
        Self {
            ordinal,
            identity,
            before,
            after,
            semantics,
            metadata,
        }
    }

    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub(crate) fn identity(&self) -> &RecordIdentity {
        &self.identity
    }

    pub(crate) fn before(&self) -> Option<&RecordData> {
        self.before.as_ref()
    }

    pub(crate) fn after(&self) -> &RecordData {
        &self.after
    }

    pub(crate) const fn semantics(&self) -> UpdateSemantics {
        self.semantics
    }

    pub(crate) fn metadata(&self) -> &UpdateMetadata {
        &self.metadata
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DeletedRecord {
    ordinal: usize,
    identity: RecordIdentity,
    before: Option<RecordData>,
}

impl DeletedRecord {
    pub(crate) fn new(
        ordinal: usize,
        identity: RecordIdentity,
        before: Option<RecordData>,
    ) -> Self {
        Self {
            ordinal,
            identity,
            before,
        }
    }

    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub(crate) fn identity(&self) -> &RecordIdentity {
        &self.identity
    }

    pub(crate) fn before(&self) -> Option<&RecordData> {
        self.before.as_ref()
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChangeContractError {
    #[error("{operation} record at ordinal {ordinal} is incompatible with schema {schema_id}")]
    SchemaMismatch {
        schema_id: &'static str,
        operation: &'static str,
        ordinal: usize,
    },
    #[error("{operation} graph record at ordinal {ordinal} has a mismatched identity")]
    IdentityMismatch {
        operation: &'static str,
        ordinal: usize,
    },
    #[error("change ordinal {ordinal} occurs more than once")]
    DuplicateOrdinal { ordinal: usize },
}

#[derive(Debug)]
pub(crate) struct ChangeSet {
    id: ChangeSetId,
    schema: SchemaRef,
    added: Arc<[AddedRecord]>,
    updated: Arc<[UpdatedRecord]>,
    deleted: Arc<[DeletedRecord]>,
}

impl ChangeSet {
    pub(crate) fn try_new(
        id: ChangeSetId,
        schema: SchemaRef,
        added: Vec<AddedRecord>,
        updated: Vec<UpdatedRecord>,
        deleted: Vec<DeletedRecord>,
    ) -> Result<ChangeSetRef, ChangeContractError> {
        let mut ordinals = HashSet::new();
        for record in &added {
            Self::validate_added(&schema, record)?;
            Self::insert_ordinal(&mut ordinals, record.ordinal)?;
        }
        for record in &updated {
            Self::validate_updated(&schema, record)?;
            Self::insert_ordinal(&mut ordinals, record.ordinal)?;
        }
        for record in &deleted {
            Self::validate_deleted(&schema, record)?;
            Self::insert_ordinal(&mut ordinals, record.ordinal)?;
        }

        Ok(Arc::new(Self {
            id,
            schema,
            added: added.into(),
            updated: updated.into(),
            deleted: deleted.into(),
        }))
    }

    fn insert_ordinal(
        ordinals: &mut HashSet<usize>,
        ordinal: usize,
    ) -> Result<(), ChangeContractError> {
        if ordinals.insert(ordinal) {
            Ok(())
        } else {
            Err(ChangeContractError::DuplicateOrdinal { ordinal })
        }
    }

    fn validate_graph_identity(
        operation: &'static str,
        ordinal: usize,
        identity: &RecordIdentity,
        record: &GraphRecord,
    ) -> Result<(), ChangeContractError> {
        match identity {
            RecordIdentity::GraphElement(identity) if identity == record.reference() => Ok(()),
            _ => Err(ChangeContractError::IdentityMismatch { operation, ordinal }),
        }
    }

    fn schema_mismatch(
        schema: &ChangeSchema,
        operation: &'static str,
        ordinal: usize,
    ) -> ChangeContractError {
        ChangeContractError::SchemaMismatch {
            schema_id: schema.id(),
            operation,
            ordinal,
        }
    }

    fn validate_added(
        schema: &ChangeSchema,
        record: &AddedRecord,
    ) -> Result<(), ChangeContractError> {
        match (schema.kind(), &record.identity, &record.after) {
            (
                ChangeSchemaKind::GraphChange,
                identity,
                RecordData::Graph(graph @ GraphRecord::Element(_)),
            ) => Self::validate_graph_identity("added", record.ordinal, identity, graph),
            (
                ChangeSchemaKind::QueryResult,
                RecordIdentity::QueryRow(_),
                RecordData::QueryRow(_),
            ) => Ok(()),
            _ => Err(Self::schema_mismatch(schema, "added", record.ordinal)),
        }
    }

    fn validate_updated(
        schema: &ChangeSchema,
        record: &UpdatedRecord,
    ) -> Result<(), ChangeContractError> {
        match (schema.kind(), &record.identity, &record.after) {
            (
                ChangeSchemaKind::GraphChange,
                identity,
                RecordData::Graph(graph @ (GraphRecord::Element(_) | GraphRecord::Future(_))),
            ) if record.semantics == UpdateSemantics::Patch
                && matches!(
                    record.before,
                    None | Some(RecordData::Graph(GraphRecord::Element(_)))
                        | Some(RecordData::Graph(GraphRecord::Metadata(_)))
                ) =>
            {
                Self::validate_graph_identity("updated", record.ordinal, identity, graph)
            }
            (
                ChangeSchemaKind::QueryResult,
                RecordIdentity::QueryRow(_),
                RecordData::QueryRow(_),
            ) if record.semantics == UpdateSemantics::Replace
                && matches!(
                    (&record.metadata, &record.before),
                    (
                        UpdateMetadata::QueryUpdate { .. },
                        Some(RecordData::QueryRow(_))
                    ) | (
                        UpdateMetadata::QueryAggregation { .. },
                        None | Some(RecordData::QueryRow(_))
                    )
                ) =>
            {
                Ok(())
            }
            _ => Err(Self::schema_mismatch(schema, "updated", record.ordinal)),
        }
    }

    fn validate_deleted(
        schema: &ChangeSchema,
        record: &DeletedRecord,
    ) -> Result<(), ChangeContractError> {
        match (schema.kind(), &record.identity, &record.before) {
            (
                ChangeSchemaKind::GraphChange,
                identity,
                Some(RecordData::Graph(
                    graph @ (GraphRecord::Element(_) | GraphRecord::Metadata(_)),
                )),
            ) => Self::validate_graph_identity("deleted", record.ordinal, identity, graph),
            (ChangeSchemaKind::GraphChange, RecordIdentity::GraphElement(_), None) => Ok(()),
            (
                ChangeSchemaKind::QueryResult,
                RecordIdentity::QueryRow(_),
                Some(RecordData::QueryRow(_)) | None,
            ) => Ok(()),
            _ => Err(Self::schema_mismatch(schema, "deleted", record.ordinal)),
        }
    }

    pub(crate) const fn id(&self) -> ChangeSetId {
        self.id
    }

    pub(crate) fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    pub(crate) fn added(&self) -> &[AddedRecord] {
        &self.added
    }

    pub(crate) fn updated(&self) -> &[UpdatedRecord] {
        &self.updated
    }

    pub(crate) fn deleted(&self) -> &[DeletedRecord] {
        &self.deleted
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.deleted.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TraceMetadata {
    trace_id: Arc<str>,
    span_id: Option<Arc<str>>,
}

impl TraceMetadata {
    pub(crate) fn new(trace_id: impl Into<Arc<str>>, span_id: Option<Arc<str>>) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id,
        }
    }

    pub(crate) fn trace_id(&self) -> &str {
        &self.trace_id
    }

    pub(crate) fn span_id(&self) -> Option<&str> {
        self.span_id.as_deref()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SystemMetadataExtensions {
    trace: Option<TraceMetadata>,
    graph_epoch: Option<u64>,
    config_epoch: Option<u64>,
}

impl SystemMetadataExtensions {
    pub(crate) fn with_trace(mut self, trace: TraceMetadata) -> Self {
        self.trace = Some(trace);
        self
    }

    pub(crate) fn with_graph_epoch(mut self, graph_epoch: u64) -> Self {
        self.graph_epoch = Some(graph_epoch);
        self
    }

    pub(crate) fn with_config_epoch(mut self, config_epoch: u64) -> Self {
        self.config_epoch = Some(config_epoch);
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SystemMetadata {
    source_id: Option<Arc<str>>,
    query_id: Option<Arc<str>>,
    sequence: Option<u64>,
    source_position: Option<Bytes>,
    timestamp: DateTime<Utc>,
    profiling: Option<Arc<ProfilingMetadata>>,
    trace: Option<TraceMetadata>,
    graph_epoch: Option<u64>,
    config_epoch: Option<u64>,
}

impl SystemMetadata {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source_id: Option<Arc<str>>,
        query_id: Option<Arc<str>>,
        sequence: Option<u64>,
        source_position: Option<Bytes>,
        timestamp: DateTime<Utc>,
        profiling: Option<ProfilingMetadata>,
        extensions: SystemMetadataExtensions,
    ) -> Self {
        Self {
            source_id,
            query_id,
            sequence,
            source_position,
            timestamp,
            profiling: profiling.map(Arc::new),
            trace: extensions.trace,
            graph_epoch: extensions.graph_epoch,
            config_epoch: extensions.config_epoch,
        }
    }

    pub(crate) fn source_id(&self) -> Option<&str> {
        self.source_id.as_deref()
    }

    pub(crate) fn query_id(&self) -> Option<&str> {
        self.query_id.as_deref()
    }

    pub(crate) const fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    pub(crate) fn source_position(&self) -> Option<&Bytes> {
        self.source_position.as_ref()
    }

    pub(crate) const fn timestamp(&self) -> DateTime<Utc> {
        self.timestamp
    }

    pub(crate) fn profiling(&self) -> Option<&ProfilingMetadata> {
        self.profiling.as_deref()
    }

    pub(crate) fn trace(&self) -> Option<&TraceMetadata> {
        self.trace.as_ref()
    }

    pub(crate) const fn graph_epoch(&self) -> Option<u64> {
        self.graph_epoch
    }

    pub(crate) const fn config_epoch(&self) -> Option<u64> {
        self.config_epoch
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ContextContributorKind {
    Runtime,
    Source,
    Query,
    Reaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContextContributor {
    kind: ContextContributorKind,
    component_id: Arc<str>,
}

impl ContextContributor {
    pub(crate) fn new(kind: ContextContributorKind, component_id: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            component_id: component_id.into(),
        }
    }

    pub(crate) const fn kind(&self) -> ContextContributorKind {
        self.kind
    }

    pub(crate) fn component_id(&self) -> &str {
        &self.component_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ContextValue {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    String(Arc<str>),
    Bytes(Arc<[u8]>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ContextContribution {
    contributor: ContextContributor,
    key: Arc<str>,
    value: ContextValue,
}

impl ContextContribution {
    pub(crate) fn new(
        contributor: ContextContributor,
        key: impl Into<Arc<str>>,
        value: ContextValue,
    ) -> Self {
        Self {
            contributor,
            key: key.into(),
            value,
        }
    }

    pub(crate) fn contributor(&self) -> &ContextContributor {
        &self.contributor
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn value(&self) -> &ContextValue {
        &self.value
    }
}

#[derive(Debug)]
pub(crate) struct ProcessingContextRoot {
    id: ContextRootId,
}

impl ProcessingContextRoot {
    pub(crate) const fn id(&self) -> ContextRootId {
        self.id
    }
}

#[derive(Debug)]
pub(crate) struct ProcessingContextNode {
    parent: Option<Arc<ProcessingContextNode>>,
    contribution: ContextContribution,
}

impl ProcessingContextNode {
    pub(crate) fn parent(&self) -> Option<&Arc<ProcessingContextNode>> {
        self.parent.as_ref()
    }

    pub(crate) fn contribution(&self) -> &ContextContribution {
        &self.contribution
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProcessingContext {
    root: Arc<ProcessingContextRoot>,
    head: Option<Arc<ProcessingContextNode>>,
    len: usize,
}

impl ProcessingContext {
    fn new(root_id: ContextRootId) -> Self {
        Self {
            root: Arc::new(ProcessingContextRoot { id: root_id }),
            head: None,
            len: 0,
        }
    }

    fn append(&self, contribution: ContextContribution) -> Self {
        Self {
            root: self.root.clone(),
            head: Some(Arc::new(ProcessingContextNode {
                parent: self.head.clone(),
                contribution,
            })),
            len: self.len + 1,
        }
    }

    pub(crate) fn root(&self) -> &Arc<ProcessingContextRoot> {
        &self.root
    }

    pub(crate) fn head(&self) -> Option<&Arc<ProcessingContextNode>> {
        self.head.as_ref()
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChangeEnvelope {
    id: EnvelopeId,
    change_set: ChangeSetRef,
    context: ProcessingContext,
    system: Arc<SystemMetadata>,
    metadata: Arc<HashMap<String, serde_json::Value>>,
}

impl ChangeEnvelope {
    pub(crate) fn new(
        change_set: ChangeSetRef,
        system: SystemMetadata,
        metadata: HashMap<String, serde_json::Value>,
    ) -> Self {
        let context_root_id = context_root_id(change_set.id(), &system);
        let context = ProcessingContext::new(context_root_id);
        let id = envelope_id(change_set.id(), &system, &metadata, context_root_id);
        Self {
            id,
            change_set,
            context,
            system: Arc::new(system),
            metadata: Arc::new(metadata),
        }
    }

    pub(crate) fn append_context(&self, contribution: ContextContribution) -> Self {
        let context = self.context.append(contribution.clone());
        let mut identity = StableIdBuilder::new("drasi.internal.change-envelope.context/v1");
        identity.u64("parent-envelope", self.id.value());
        identity.context_contribution(&contribution);

        Self {
            id: EnvelopeId::new(identity.finish()),
            change_set: self.change_set.clone(),
            context,
            system: self.system.clone(),
            metadata: self.metadata.clone(),
        }
    }

    pub(crate) const fn id(&self) -> EnvelopeId {
        self.id
    }

    pub(crate) fn change_set(&self) -> &ChangeSetRef {
        &self.change_set
    }

    pub(crate) fn context(&self) -> &ProcessingContext {
        &self.context
    }

    pub(crate) fn system(&self) -> &SystemMetadata {
        &self.system
    }

    pub(crate) fn metadata(&self) -> &HashMap<String, serde_json::Value> {
        &self.metadata
    }
}

pub(super) struct StableIdBuilder {
    state: u64,
}

impl StableIdBuilder {
    pub(super) fn new(domain: &str) -> Self {
        let mut builder = Self {
            state: 0xcbf2_9ce4_8422_2325,
        };
        builder.bytes("domain", domain.as_bytes());
        builder
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub(super) fn bytes(&mut self, label: &str, value: &[u8]) {
        self.update(&(label.len() as u64).to_le_bytes());
        self.update(label.as_bytes());
        self.update(&(value.len() as u64).to_le_bytes());
        self.update(value);
    }

    pub(super) fn optional_bytes(&mut self, label: &str, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.bytes("option", &[1]);
                self.bytes(label, value);
            }
            None => self.bytes("option", &[0]),
        }
    }

    pub(super) fn string(&mut self, label: &str, value: &str) {
        self.bytes(label, value.as_bytes());
    }

    pub(super) fn optional_string(&mut self, label: &str, value: Option<&str>) {
        self.optional_bytes(label, value.map(str::as_bytes));
    }

    pub(super) fn u64(&mut self, label: &str, value: u64) {
        self.bytes(label, &value.to_le_bytes());
    }

    pub(super) fn optional_u64(&mut self, label: &str, value: Option<u64>) {
        match value {
            Some(value) => {
                self.bytes("option", &[1]);
                self.u64(label, value);
            }
            None => self.bytes("option", &[0]),
        }
    }

    fn i64(&mut self, label: &str, value: i64) {
        self.bytes(label, &value.to_le_bytes());
    }

    fn bool(&mut self, label: &str, value: bool) {
        self.bytes(label, &[u8::from(value)]);
    }

    fn system(&mut self, system: &SystemMetadata) {
        self.optional_string("source-id", system.source_id());
        self.optional_string("query-id", system.query_id());
        self.optional_u64("sequence", system.sequence());
        self.optional_bytes(
            "source-position",
            system.source_position().map(Bytes::as_ref),
        );
        self.i64("timestamp-seconds", system.timestamp().timestamp());
        self.u64(
            "timestamp-nanos",
            u64::from(system.timestamp().timestamp_subsec_nanos()),
        );
        self.optional_u64("graph-epoch", system.graph_epoch());
        self.optional_u64("config-epoch", system.config_epoch());
        if let Some(trace) = system.trace() {
            self.bytes("trace-present", &[1]);
            self.string("trace-id", trace.trace_id());
            self.optional_string("span-id", trace.span_id());
        } else {
            self.bytes("trace-present", &[0]);
        }
        if let Some(profiling) = system.profiling() {
            self.bytes("profiling-present", &[1]);
            for (label, value) in [
                ("source-ns", profiling.source_ns),
                ("reactivator-start-ns", profiling.reactivator_start_ns),
                ("reactivator-end-ns", profiling.reactivator_end_ns),
                ("source-receive-ns", profiling.source_receive_ns),
                ("source-send-ns", profiling.source_send_ns),
                ("query-receive-ns", profiling.query_receive_ns),
                ("query-core-call-ns", profiling.query_core_call_ns),
                ("query-core-return-ns", profiling.query_core_return_ns),
                ("query-send-ns", profiling.query_send_ns),
                ("reaction-receive-ns", profiling.reaction_receive_ns),
                ("reaction-complete-ns", profiling.reaction_complete_ns),
            ] {
                self.optional_u64(label, value);
            }
        } else {
            self.bytes("profiling-present", &[0]);
        }
    }

    fn json(&mut self, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => self.bytes("json-kind", b"null"),
            serde_json::Value::Bool(value) => {
                self.bytes("json-kind", b"bool");
                self.bool("json-bool", *value);
            }
            serde_json::Value::Number(value) => {
                self.bytes("json-kind", b"number");
                self.string("json-number", &value.to_string());
            }
            serde_json::Value::String(value) => {
                self.bytes("json-kind", b"string");
                self.string("json-string", value);
            }
            serde_json::Value::Array(values) => {
                self.bytes("json-kind", b"array");
                self.u64("json-array-len", values.len() as u64);
                for value in values {
                    self.json(value);
                }
            }
            serde_json::Value::Object(values) => {
                self.bytes("json-kind", b"object");
                self.u64("json-object-len", values.len() as u64);
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_unstable_by_key(|(key, _)| *key);
                for (key, value) in entries {
                    self.string("json-key", key);
                    self.json(value);
                }
            }
        }
    }

    fn metadata(&mut self, metadata: &HashMap<String, serde_json::Value>) {
        let mut entries: Vec<_> = metadata.iter().collect();
        entries.sort_unstable_by_key(|(key, _)| *key);
        self.u64("metadata-len", entries.len() as u64);
        for (key, value) in entries {
            self.string("metadata-key", key);
            self.json(value);
        }
    }

    fn context_contribution(&mut self, contribution: &ContextContribution) {
        let kind = match contribution.contributor().kind() {
            ContextContributorKind::Runtime => "runtime",
            ContextContributorKind::Source => "source",
            ContextContributorKind::Query => "query",
            ContextContributorKind::Reaction => "reaction",
        };
        self.string("context-kind", kind);
        self.string(
            "context-component",
            contribution.contributor().component_id(),
        );
        self.string("context-key", contribution.key());
        match contribution.value() {
            ContextValue::Bool(value) => {
                self.string("context-value-kind", "bool");
                self.bool("context-value", *value);
            }
            ContextValue::Signed(value) => {
                self.string("context-value-kind", "signed");
                self.i64("context-value", *value);
            }
            ContextValue::Unsigned(value) => {
                self.string("context-value-kind", "unsigned");
                self.u64("context-value", *value);
            }
            ContextValue::String(value) => {
                self.string("context-value-kind", "string");
                self.string("context-value", value);
            }
            ContextValue::Bytes(value) => {
                self.string("context-value-kind", "bytes");
                self.bytes("context-value", value);
            }
        }
    }

    pub(super) const fn finish(self) -> u64 {
        self.state
    }
}

fn context_root_id(change_set_id: ChangeSetId, system: &SystemMetadata) -> ContextRootId {
    let mut identity = StableIdBuilder::new("drasi.internal.processing-context/v1");
    identity.u64("change-set", change_set_id.value());
    identity.system(system);
    ContextRootId::new(identity.finish())
}

fn envelope_id(
    change_set_id: ChangeSetId,
    system: &SystemMetadata,
    metadata: &HashMap<String, serde_json::Value>,
    context_root_id: ContextRootId,
) -> EnvelopeId {
    let mut identity = StableIdBuilder::new("drasi.internal.change-envelope/v1");
    identity.u64("change-set", change_set_id.value());
    identity.u64("context-root", context_root_id.value());
    identity.system(system);
    identity.metadata(metadata);
    EnvelopeId::new(identity.finish())
}

#[cfg(test)]
mod tests;
