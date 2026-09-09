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

use std::sync::{Arc, OnceLock};

use bincode::Options;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
use serde::{Deserialize, Serialize};

use crate::{
    channels::SourceEventWrapper, computation::internal::typed_change,
    profiling::ProfilingMetadata, schema::SourceSchema,
};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId, ComponentId, ContextEntry,
    ContextValue, ContractError, Record, RecordId, RecordImage, RecordReference,
    RecordValidationError, RecordValidator, Schema, SchemaDescriptor, SchemaId, SchemaVersion,
    StreamId, SystemMetadata, UpdateSemantics,
};

const SOURCE_METADATA: &str = "drasi.legacy-source.v1";
const GRAPH_IDENTITY: &str = "drasi.graph-element";

#[derive(Debug, thiserror::Error)]
pub enum GraphCodecError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("invalid typed graph record: {0}")]
    Encoding(#[from] Box<bincode::ErrorKind>),
    #[error("invalid source metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("legacy boundary conversion failed: {0}")]
    Boundary(#[source] anyhow::Error),
    #[error("graph record operation or image is incompatible with SourceChange")]
    Operation,
    #[error("identity-only graph deletion requires a nonnegative event timestamp")]
    MissingEventTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacySourceMetadata {
    pub version: u32,
    pub source_id: String,
    pub sequence: Option<u64>,
    pub source_position: Option<Vec<u8>>,
    pub timestamp: DateTime<Utc>,
    pub profiling: Option<ProfilingMetadata>,
    pub schema: Option<SourceSchema>,
}

pub struct GraphChangeCodec;

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Box<bincode::ErrorKind>> {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(bytes.len() as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)
}

impl GraphChangeCodec {
    pub fn schema() -> Arc<Schema> {
        static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
        SCHEMA.get_or_init(|| Arc::new(Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("drasi.graph-change").expect("constant schema id"),
                SchemaVersion::try_new(1).expect("constant schema version"),
                "drasi-source-change-bincode1",
                Bytes::from_static(b"SourceChange-v1;ElementReference-key;insert/full;update/patch;delete/partial;future/patch"),
            ).expect("constant graph schema"),
            Arc::new(GraphValidator),
        ))).clone()
    }

    pub fn source_metadata(
        envelope: &ChangeEnvelope,
    ) -> Result<Option<LegacySourceMetadata>, GraphCodecError> {
        let Some(entry) = envelope
            .annotations()
            .entries()
            .filter(|entry| entry.key() == SOURCE_METADATA)
            .last()
        else {
            return Ok(None);
        };
        let ContextValue::Bytes(bytes) = entry.value() else {
            return Err(GraphCodecError::Operation);
        };
        let metadata: LegacySourceMetadata = serde_json::from_slice(&bytes)?;
        if metadata.version != 1 {
            return Err(GraphCodecError::Operation);
        }
        Ok(Some(metadata))
    }

    /// Preserve the raw legacy fields separately from the adapter's stream/sequence.
    /// The A2 owned ingress path is used when the source event has a sole owner.
    pub fn encode_source_event(
        event: Arc<SourceEventWrapper>,
        component: &ComponentId,
        stream: StreamId,
        sequence: u64,
        source_schema: Option<SourceSchema>,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        let metadata = LegacySourceMetadata {
            version: 1,
            source_id: event.source_id.clone(),
            sequence: event.sequence,
            source_position: event
                .source_position
                .as_ref()
                .map(|position| position.to_vec()),
            timestamp: event.timestamp,
            profiling: event.profiling.clone(),
            schema: source_schema,
        };
        let typed = match Arc::try_unwrap(event) {
            Ok(event) => {
                typed_change::source_event_parts_to_envelope(event.into_parts(), Default::default())
            }
            Err(event) => typed_change::source_event_to_envelope(&event, Default::default()),
        }
        .map_err(|error| GraphCodecError::Boundary(error.into()))?;
        let advisory_identity = typed.change_set().id().value();
        let change = typed_change::source_change_from_envelope_owned(typed)
            .map_err(|error| GraphCodecError::Boundary(error.into()))?;
        let mut envelope = Self::encode_change_with_position(
            change,
            stream,
            sequence,
            Some(metadata.timestamp),
            metadata
                .source_position
                .as_ref()
                .map(|position| Bytes::copy_from_slice(position)),
        )?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            SOURCE_METADATA,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&metadata)?)),
        )?)?;
        envelope.append_annotation(ContextEntry::try_new(
            component.clone(),
            "drasi.advisory-change-identity",
            ContextValue::Unsigned(advisory_identity),
        )?)?;
        Ok(envelope)
    }

    pub fn encode_change(
        change: SourceChange,
        stream: StreamId,
        sequence: u64,
        timestamp: Option<DateTime<Utc>>,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        Self::encode_change_with_position(change, stream, sequence, timestamp, None)
    }

    fn encode_change_with_position(
        change: SourceChange,
        stream: StreamId,
        sequence: u64,
        timestamp: Option<DateTime<Utc>>,
        position: Option<Bytes>,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        let schema = Self::schema();
        let identity = RecordId::try_new(
            GRAPH_IDENTITY,
            Bytes::from(bincode::serialize(change.get_reference())?),
        )?;
        let image = match &change {
            SourceChange::Insert { .. } => RecordImage::Full,
            SourceChange::Update { .. } | SourceChange::Future { .. } => RecordImage::Patch,
            SourceChange::Delete { .. } => RecordImage::Partial,
        };
        let record = Record::try_new(
            &schema,
            identity,
            image,
            Bytes::from(bincode::serialize(&change)?),
        )?;
        let operation = match change {
            SourceChange::Insert { .. } => ChangeOperation::Added {
                ordinal: 0,
                after: record,
            },
            SourceChange::Update { .. } | SourceChange::Future { .. } => ChangeOperation::Updated {
                ordinal: 0,
                before: None,
                after: record,
                semantics: UpdateSemantics::Patch,
            },
            SourceChange::Delete { .. } => ChangeOperation::Deleted {
                ordinal: 0,
                identity: record.reference().clone(),
                before: Some(record),
            },
        };
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                stream.as_str(),
                Bytes::copy_from_slice(&sequence.to_be_bytes()),
            )?,
            schema.descriptor().clone(),
            vec![operation],
        )?;
        let id = super::emission_id(&stream, sequence)?;
        let mut system = SystemMetadata::new(stream, sequence);
        if let Some(timestamp) = timestamp {
            system = system.with_timestamp(timestamp);
        }
        if let Some(position) = position {
            system = system.with_source_position(position);
        }
        Ok(ChangeEnvelope::new(id, changes, system))
    }

    pub fn decode_changes(envelope: &ChangeEnvelope) -> Result<Vec<SourceChange>, GraphCodecError> {
        super::data::validate_schema(Self::schema().descriptor(), envelope.changes().schema())?;
        let mut changes = Vec::new();
        for operation in envelope.changes().operations() {
            match operation {
                ChangeOperation::Added { after, .. } => {
                    let change = decode(after.payload())?;
                    if !matches!(change, SourceChange::Insert { .. }) {
                        return Err(GraphCodecError::Operation);
                    }
                    changes.push(change);
                }
                ChangeOperation::Updated {
                    after,
                    semantics: UpdateSemantics::Patch,
                    ..
                } => {
                    let change = decode(after.payload())?;
                    if !matches!(
                        change,
                        SourceChange::Update { .. } | SourceChange::Future { .. }
                    ) {
                        return Err(GraphCodecError::Operation);
                    }
                    changes.push(change);
                }
                ChangeOperation::Updated {
                    after,
                    semantics: UpdateSemantics::Replace,
                    ..
                } => {
                    let SourceChange::Insert { element } = decode(after.payload())? else {
                        return Err(GraphCodecError::Operation);
                    };
                    changes.push(SourceChange::Delete {
                        metadata: element.get_metadata().clone(),
                    });
                    changes.push(SourceChange::Insert { element });
                }
                ChangeOperation::Deleted {
                    identity, before, ..
                } => {
                    let metadata = match before {
                        Some(before) => match decode(before.payload())? {
                            SourceChange::Delete { metadata } => metadata,
                            SourceChange::Insert { element } => ElementMetadata {
                                effective_from: Self::event_time(envelope)?,
                                ..element.get_metadata().clone()
                            },
                            _ => return Err(GraphCodecError::Operation),
                        },
                        None => ElementMetadata {
                            reference: decode(identity.identity().value())?,
                            labels: Arc::from([]),
                            effective_from: Self::event_time(envelope)?,
                        },
                    };
                    changes.push(SourceChange::Delete { metadata });
                }
            }
        }
        Ok(changes)
    }

    fn event_time(envelope: &ChangeEnvelope) -> Result<u64, GraphCodecError> {
        envelope
            .system()
            .timestamp()
            .and_then(|timestamp| timestamp.timestamp_millis().try_into().ok())
            .ok_or(GraphCodecError::MissingEventTime)
    }
}

struct GraphValidator;

impl RecordValidator for GraphValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> Result<(), RecordValidationError> {
        if schema != GraphChangeCodec::schema().descriptor()
            || identity.namespace() != GRAPH_IDENTITY
        {
            return Err(RecordValidationError::new(
                "schema",
                "expected graph-change identity",
            ));
        }
        decode::<ElementReference>(identity.value())
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
        let reference: ElementReference = decode(identity.value())
            .map_err(|error| RecordValidationError::new("identity", error.to_string()))?;
        let change: SourceChange = decode(payload)
            .map_err(|error| RecordValidationError::new("record", error.to_string()))?;
        let expected = match &change {
            SourceChange::Insert { .. } => RecordImage::Full,
            SourceChange::Update { .. } | SourceChange::Future { .. } => RecordImage::Patch,
            SourceChange::Delete { .. } => RecordImage::Partial,
        };
        if change.get_reference() != &reference || expected != image {
            return Err(RecordValidationError::new(
                "image",
                "graph identity/image does not match its typed payload",
            ));
        }
        Ok(())
    }
}
