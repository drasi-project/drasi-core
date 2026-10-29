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

use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core_types::{
    interface::FutureElementRef,
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use serde::{Deserialize, Serialize};

use crate::{
    binary_codec::{decode_bounded_messagepack, encode_bounded_messagepack},
    exchange::SourceEventWrapper,
    internal::typed_change,
    profiling::ProfilingMetadata,
    schema::SourceSchema,
    BinaryEnvelopeCodecError, ControlMessage, ElementImage, ElementImageError, ElementPatch,
    ElementPatchError,
};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId, ChangeSetRef, ComponentId,
    ContextEntry, ContextValue, ContractError, Record, RecordId, RecordImage, RecordReference,
    RecordValidationError, RecordValidator, Schema, SchemaDescriptor, SchemaId, SchemaVersion,
    StreamId, SystemMetadata, UpdateSemantics,
};

const SOURCE_METADATA: &str = "drasi.legacy-source.v1";
pub(crate) const GRAPH_IDENTITY: &str = "drasi.graph-element";
const GRAPH_CONTROL: &str = "drasi.graph-control.v1";

pub(crate) mod result_change;
mod result_node;
#[cfg(test)]
mod tests;

pub use result_node::{
    ResultNodeError, ResultNodeProjector, ResultOperationMetadata, ResultProjection,
};

#[derive(Debug, thiserror::Error)]
pub enum GraphCodecError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("invalid typed graph record: {0}")]
    Encoding(#[from] BinaryEnvelopeCodecError),
    #[error(transparent)]
    Image(#[from] ElementImageError),
    #[error(transparent)]
    Patch(Box<ElementPatchError>),
    #[error("invalid source metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("legacy boundary conversion failed: {0}")]
    Boundary(#[source] anyhow::Error),
    #[error("graph record operation or image is incompatible with SourceChange")]
    Operation,
    #[error("identity-only graph deletion requires a nonnegative event timestamp")]
    MissingEventTime,
    #[error("invalid graph stream control: {0}")]
    Control(&'static str),
}

impl From<ElementPatchError> for GraphCodecError {
    fn from(error: ElementPatchError) -> Self {
        Self::Patch(Box::new(error))
    }
}

/// Known data preceding a change. Metadata alone does not invent an element kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementBeforeImage {
    Metadata(ElementMetadata),
    Element(ElementImage),
}

impl ElementBeforeImage {
    pub fn metadata(&self) -> &ElementMetadata {
        match self {
            Self::Metadata(metadata) => metadata,
            Self::Element(image) => image.metadata(),
        }
    }

    pub fn validate(&self) -> Result<(), ElementImageError> {
        if let Self::Element(image) = self {
            image.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderedFuture {
    ordinal: u64,
    future_ref: FutureElementRef,
}

#[derive(Debug, Serialize, Deserialize)]
enum GraphControl {
    FuturesDue,
    Reevaluate(Vec<OrderedFuture>),
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

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, BinaryEnvelopeCodecError> {
    decode_bounded_messagepack(bytes, bytes.len())
}

fn encode<T: Serialize>(value: &T) -> Result<Bytes, BinaryEnvelopeCodecError> {
    // The enclosing transport applies the configured frame-size limit.
    encode_bounded_messagepack(value, usize::MAX).map(Bytes::from)
}

impl GraphChangeCodec {
    pub fn encode_futures_due(
        stream: StreamId,
        sequence: u64,
        timestamp: DateTime<Utc>,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        let changes = super::ChangeSet::try_new(
            super::ChangeSetId::try_new(
                stream.as_str(),
                Bytes::copy_from_slice(&sequence.to_be_bytes()),
            )?,
            Self::schema().descriptor().clone(),
            Vec::new(),
        )?;
        Ok(ChangeEnvelope::new(
            super::emission_id(&stream, sequence)?,
            changes,
            super::SystemMetadata::new(stream, sequence)
                .with_timestamp(timestamp)
                .with_control(ControlMessage::try_new(
                    GRAPH_CONTROL,
                    encode(&GraphControl::FuturesDue)?,
                )?),
        ))
    }

    pub fn is_futures_due(envelope: &ChangeEnvelope) -> bool {
        envelope.changes().schema() == Self::schema().descriptor()
            && envelope.changes().operations().is_empty()
            && matches!(Self::control(envelope), Ok(Some(GraphControl::FuturesDue)))
    }

    fn control(envelope: &ChangeEnvelope) -> Result<Option<GraphControl>, GraphCodecError> {
        let Some(control) = envelope.system().control() else {
            return Ok(None);
        };
        if control.kind() != GRAPH_CONTROL {
            return Err(GraphCodecError::Control("unexpected control kind"));
        }
        Ok(Some(decode(control.payload())?))
    }

    pub fn schema() -> Arc<Schema> {
        static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
        SCHEMA.get_or_init(|| Arc::new(Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("drasi.graph-change").expect("constant schema id"),
                SchemaVersion::try_new(1).expect("constant schema version"),
                "drasi-element-msgpack-v1",
                Bytes::from_static(b"ElementImage-v1;ElementReference-msgpack-key;add/full;replace/full;patch/ElementPatch;delete/ElementBeforeImage-or-identity;event-local-graph-controls-v1"),
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
            sequence: Some(event.sequence),
            source_position: event
                .source_position
                .as_ref()
                .map(|position| position.to_vec()),
            timestamp: event.timestamp,
            profiling: event.profiling.clone(),
            schema: source_schema,
        };
        if matches!(
            &event.event,
            crate::exchange::SourceEvent::Control(crate::exchange::SourceControl::FuturesDue)
        ) {
            let mut envelope = Self::encode_futures_due(stream, sequence, metadata.timestamp)?;
            envelope.append_annotation(ContextEntry::try_new(
                component.clone(),
                SOURCE_METADATA,
                ContextValue::Bytes(Arc::from(serde_json::to_vec(&metadata)?)),
            )?)?;
            return Ok(envelope);
        }
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

    /// Encode an ordered batch, including an empty progress-only batch.
    pub fn encode_changes(
        changes: &[SourceChange],
        stream: StreamId,
        sequence: u64,
        timestamp: Option<DateTime<Utc>>,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        let (changes, control) = Self::encode_change_set(changes, &stream, sequence)?;
        let id = super::emission_id(&stream, sequence)?;
        let mut system = SystemMetadata::new(stream, sequence);
        if let Some(timestamp) = timestamp {
            system = system.with_timestamp(timestamp);
        }
        if let Some(control) = control {
            system = system.with_control(control);
        }
        Ok(ChangeEnvelope::new(id, changes, system))
    }

    /// Emit transformed graph changes while preserving the input's annotations,
    /// timestamp, ordering policy, source position and lineage. The output has its own identity.
    /// Inherited source metadata remains provenance; downstream graph progress
    /// uses the new stream's sequence, or an explicit immediate producer marker.
    pub fn derive_changes(
        input: &ChangeEnvelope,
        changes: &[SourceChange],
        stream: StreamId,
        sequence: u64,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        Self::derive_changes_with_ordering(
            input,
            changes,
            stream,
            sequence,
            input.system().ordering(),
        )
    }

    /// Choose the new producer's ordering explicitly. Producer-sequence output
    /// must also carry that producer's own progress identity, not an ancestor's.
    pub fn derive_changes_with_ordering(
        input: &ChangeEnvelope,
        changes: &[SourceChange],
        stream: StreamId,
        sequence: u64,
        ordering: crate::ChangeOrdering,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        let (changes, control) = Self::encode_change_set(changes, &stream, sequence)?;
        let id = super::emission_id(&stream, sequence)?;
        let mut system = SystemMetadata::new(stream, sequence).with_ordering(ordering);
        if let Some(timestamp) = input.system().timestamp() {
            system = system.with_timestamp(timestamp);
        }
        if let Some(position) = input.system().source_position() {
            system = system.with_source_position(position.clone());
        }
        if let Some(control) = control {
            system = system.with_control(control);
        }
        Ok(input.derive(id, changes, system))
    }

    fn encode_change_with_position(
        change: SourceChange,
        stream: StreamId,
        sequence: u64,
        timestamp: Option<DateTime<Utc>>,
        position: Option<Bytes>,
    ) -> Result<ChangeEnvelope, GraphCodecError> {
        let (changes, control) =
            Self::encode_change_set(std::slice::from_ref(&change), &stream, sequence)?;
        let id = super::emission_id(&stream, sequence)?;
        let mut system = SystemMetadata::new(stream, sequence);
        if let Some(timestamp) = timestamp {
            system = system.with_timestamp(timestamp);
        }
        if let Some(position) = position {
            system = system.with_source_position(position);
        }
        if let Some(control) = control {
            system = system.with_control(control);
        }
        Ok(ChangeEnvelope::new(id, changes, system))
    }

    fn encode_change_set(
        changes: &[SourceChange],
        stream: &StreamId,
        sequence: u64,
    ) -> Result<(ChangeSetRef, Option<ControlMessage>), GraphCodecError> {
        let schema = Self::schema();
        let mut futures = Vec::new();
        let mut operations = Vec::with_capacity(changes.len());
        for (ordinal, change) in changes.iter().enumerate() {
            let ordinal = u64::try_from(ordinal).map_err(|_| GraphCodecError::Operation)?;
            if let SourceChange::Future { future_ref } = change {
                futures.push(OrderedFuture {
                    ordinal,
                    future_ref: future_ref.clone(),
                });
            } else {
                operations.push(Self::encode_operation(ordinal, change)?);
            }
        }
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                stream.as_str(),
                Bytes::copy_from_slice(&sequence.to_be_bytes()),
            )?,
            schema.descriptor().clone(),
            operations,
        )?;
        let control = if futures.is_empty() {
            None
        } else {
            Some(ControlMessage::try_new(
                GRAPH_CONTROL,
                encode(&GraphControl::Reevaluate(futures))?,
            )?)
        };
        Ok((changes, control))
    }

    /// Encode one data change as its ordinary graph operation. Scheduled
    /// notifications are event-local controls, never graph records.
    pub(crate) fn encode_operation(
        ordinal: u64,
        change: &SourceChange,
    ) -> Result<ChangeOperation, GraphCodecError> {
        Ok(match change {
            SourceChange::Insert { element } => ChangeOperation::Added {
                ordinal,
                after: Self::element_record(&ElementImage::try_from(element)?)?,
            },
            SourceChange::Replace { element } => ChangeOperation::Updated {
                ordinal,
                before: None,
                after: Self::element_record(&ElementImage::try_from(element)?)?,
                semantics: UpdateSemantics::Replace,
            },
            SourceChange::Update {
                element,
                removed_properties,
            } => ChangeOperation::Updated {
                ordinal,
                before: None,
                after: Self::patch_record(&ElementPatch::try_new(
                    ElementImage::try_from(element)?,
                    removed_properties.clone(),
                )?)?,
                semantics: UpdateSemantics::Patch,
            },
            SourceChange::Delete { metadata } => {
                let record = Self::partial_record(&ElementBeforeImage::Metadata(metadata.clone()))?;
                ChangeOperation::Deleted {
                    ordinal,
                    identity: record.reference().clone(),
                    before: Some(record),
                }
            }
            SourceChange::Future { .. } => return Err(GraphCodecError::Operation),
        })
    }

    pub fn element_reference(
        reference: &ElementReference,
    ) -> Result<RecordReference, GraphCodecError> {
        Ok(RecordReference::try_new(
            &Self::schema(),
            RecordId::try_new(GRAPH_IDENTITY, encode(reference)?)?,
        )?)
    }

    pub fn element_record(image: &ElementImage) -> Result<Record, GraphCodecError> {
        image.validate()?;
        Ok(Record::try_new(
            &Self::schema(),
            Self::element_reference(&image.metadata().reference)?
                .identity()
                .clone(),
            RecordImage::Full,
            encode(image)?,
        )?)
    }

    pub fn patch_record(patch: &ElementPatch) -> Result<Record, GraphCodecError> {
        patch.validate()?;
        Ok(Record::try_new(
            &Self::schema(),
            Self::element_reference(patch.reference())?
                .identity()
                .clone(),
            RecordImage::Patch,
            encode(patch)?,
        )?)
    }

    pub fn partial_record(image: &ElementBeforeImage) -> Result<Record, GraphCodecError> {
        image.validate()?;
        Ok(Record::try_new(
            &Self::schema(),
            Self::element_reference(&image.metadata().reference)?
                .identity()
                .clone(),
            RecordImage::Partial,
            encode(image)?,
        )?)
    }

    pub fn decode_element(record: &Record) -> Result<ElementImage, GraphCodecError> {
        if record.image() != RecordImage::Full {
            return Err(GraphCodecError::Operation);
        }
        let image = ElementImage::decode(record.payload(), record.payload().len())?;
        Self::validate_record_reference(record, &image.metadata().reference)?;
        Ok(image)
    }

    pub fn decode_patch(record: &Record) -> Result<ElementPatch, GraphCodecError> {
        if record.image() != RecordImage::Patch {
            return Err(GraphCodecError::Operation);
        }
        let patch = ElementPatch::decode(record.payload(), record.payload().len())?;
        Self::validate_record_reference(record, patch.reference())?;
        Ok(patch)
    }

    pub fn decode_partial(record: &Record) -> Result<ElementBeforeImage, GraphCodecError> {
        if record.image() != RecordImage::Partial {
            return Err(GraphCodecError::Operation);
        }
        let image: ElementBeforeImage = decode(record.payload())?;
        image.validate()?;
        Self::validate_record_reference(record, &image.metadata().reference)?;
        Ok(image)
    }

    fn validate_record_reference(
        record: &Record,
        reference: &ElementReference,
    ) -> Result<(), GraphCodecError> {
        let expected = Self::element_reference(reference)?;
        if record.reference() != &expected {
            return Err(GraphCodecError::Operation);
        }
        Ok(())
    }

    pub fn decode_changes(envelope: &ChangeEnvelope) -> Result<Vec<SourceChange>, GraphCodecError> {
        super::data::validate_schema(Self::schema().descriptor(), envelope.changes().schema())?;
        let mut changes = Self::decode_operations(envelope.changes().operations(), &|| {
            Self::event_time(envelope)
        })?;
        match Self::control(envelope)? {
            Some(GraphControl::FuturesDue) if !changes.is_empty() => {
                return Err(GraphCodecError::Control("futures-due cannot carry data"));
            }
            Some(GraphControl::Reevaluate(futures)) => {
                if futures.is_empty() {
                    return Err(GraphCodecError::Control("empty reevaluation batch"));
                }
                if futures
                    .windows(2)
                    .any(|pair| pair[0].ordinal >= pair[1].ordinal)
                {
                    return Err(GraphCodecError::Control(
                        "reevaluation ordinals must be increasing",
                    ));
                }
                changes.extend(futures.into_iter().map(|future| {
                    (
                        future.ordinal,
                        SourceChange::Future {
                            future_ref: future.future_ref,
                        },
                    )
                }));
                changes.sort_by_key(|(ordinal, _)| *ordinal);
                if changes
                    .iter()
                    .enumerate()
                    .any(|(index, (ordinal, _))| u64::try_from(index).ok() != Some(*ordinal))
                {
                    return Err(GraphCodecError::Control(
                        "reevaluation ordinals must form one complete ordered batch",
                    ));
                }
            }
            _ => {}
        }
        Ok(changes.into_iter().map(|(_, change)| change).collect())
    }

    /// Decode ordered data operations without control or envelope context. The
    /// caller supplies the event time that identity-only deletions require.
    pub(crate) fn decode_operations(
        operations: &[ChangeOperation],
        event_time: &dyn Fn() -> Result<u64, GraphCodecError>,
    ) -> Result<Vec<(u64, SourceChange)>, GraphCodecError> {
        let mut changes = Vec::with_capacity(operations.len());
        for operation in operations {
            let ordinal = operation.ordinal();
            match operation {
                ChangeOperation::Added { after, .. } => {
                    changes.push((
                        ordinal,
                        SourceChange::Insert {
                            element: Element::try_from(Self::decode_element(after)?)?,
                        },
                    ));
                }
                ChangeOperation::Updated {
                    after,
                    semantics: UpdateSemantics::Patch,
                    ..
                } => {
                    let patch = Self::decode_patch(after)?;
                    changes.push((
                        ordinal,
                        SourceChange::Update {
                            element: Element::try_from(patch.image())?,
                            removed_properties: patch.removed_properties().clone(),
                        },
                    ));
                }
                ChangeOperation::Updated {
                    after,
                    semantics: UpdateSemantics::Replace,
                    ..
                } => {
                    changes.push((
                        ordinal,
                        SourceChange::Replace {
                            element: Element::try_from(Self::decode_element(after)?)?,
                        },
                    ));
                }
                ChangeOperation::Deleted {
                    identity, before, ..
                } => {
                    let metadata = match before {
                        Some(before) => match before.image() {
                            RecordImage::Full => ElementMetadata {
                                effective_from: event_time()?,
                                ..Self::decode_element(before)?.metadata().clone()
                            },
                            RecordImage::Partial => match Self::decode_partial(before)? {
                                ElementBeforeImage::Metadata(metadata) => metadata,
                                ElementBeforeImage::Element(image) => ElementMetadata {
                                    effective_from: event_time()?,
                                    ..image.metadata().clone()
                                },
                            },
                            RecordImage::Patch => return Err(GraphCodecError::Operation),
                        },
                        None => ElementMetadata {
                            reference: decode(identity.identity().value())?,
                            labels: Arc::from([]),
                            effective_from: event_time()?,
                        },
                    };
                    changes.push((ordinal, SourceChange::Delete { metadata }));
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
        let reference = decode::<ElementReference>(identity.value())
            .map_err(|error| RecordValidationError::new("identity", error.to_string()))?;
        let canonical = encode(&reference)
            .map_err(|error| RecordValidationError::new("identity", error.to_string()))?;
        if canonical != identity.value() {
            return Err(RecordValidationError::new(
                "identity",
                "graph element key must use canonical named MessagePack",
            ));
        }
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
        let payload_reference = match image {
            RecordImage::Full => ElementImage::decode(payload, payload.len())
                .map(|image| image.metadata().reference.clone())
                .map_err(|error| error.to_string()),
            RecordImage::Patch => ElementPatch::decode(payload, payload.len())
                .map(|patch| patch.reference().clone())
                .map_err(|error| error.to_string()),
            RecordImage::Partial => decode::<ElementBeforeImage>(payload)
                .map_err(|error| error.to_string())
                .and_then(|image| {
                    image.validate().map_err(|error| error.to_string())?;
                    Ok(image.metadata().reference.clone())
                }),
        }
        .map_err(|error| RecordValidationError::new("record", error))?;
        if payload_reference != reference {
            return Err(RecordValidationError::new(
                "image",
                "graph identity/image does not match its typed payload",
            ));
        }
        Ok(())
    }
}
