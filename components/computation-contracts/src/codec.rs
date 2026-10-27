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

//! Explicit, versioned computation envelope storage. This is not a serializer
//! for legacy SourceChange/QueryResult or any plugin ABI.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    data::validate_schema, ChangeEnvelope, ChangeOperation, ChangeOrdering, ChangeSet, ChangeSetId,
    ComponentId, ContextEntry, ContextValue, ContractError, ControlMessage, EnvelopeId, Lineage,
    Record, RecordId, RecordImage, RecordReference, Schema, SchemaDescriptor, SchemaId,
    SchemaVersion, StreamId, SystemMetadata, UpdateSemantics,
};

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeCodecError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
    #[error("unsupported computation envelope format version {0}")]
    Version(u32),
    #[error("computation envelope exceeds the configured {limit}-byte storage limit")]
    SizeLimit { limit: usize },
    #[error("unknown or conflicting registered schema {id} version {version}")]
    Schema { id: String, version: u32 },
}

pub struct EnvelopeCodec {
    schemas: BTreeMap<(String, u32), Arc<Schema>>,
    max_bytes: NonZeroUsize,
}

impl EnvelopeCodec {
    pub fn new(max_bytes: NonZeroUsize) -> Self {
        Self {
            schemas: BTreeMap::new(),
            max_bytes,
        }
    }

    pub fn register_schema(&mut self, schema: Arc<Schema>) -> Result<(), EnvelopeCodecError> {
        let descriptor = schema.descriptor();
        let key = (
            descriptor.id().as_str().to_owned(),
            descriptor.version().value(),
        );
        if self.schemas.contains_key(&key) {
            return Err(EnvelopeCodecError::Schema {
                id: key.0,
                version: key.1,
            });
        }
        self.schemas.insert(key, schema);
        Ok(())
    }

    pub(super) fn lookup_schema(
        &self,
        id: &str,
        version: u32,
    ) -> Result<&Schema, EnvelopeCodecError> {
        let key = (id.to_owned(), version);
        self.schemas
            .get(&key)
            .ok_or_else(|| EnvelopeCodecError::Schema {
                id: key.0,
                version: key.1,
            })
            .map(AsRef::as_ref)
    }

    pub(super) fn schema(
        &self,
        descriptor: &SchemaDescriptor,
    ) -> Result<&Schema, EnvelopeCodecError> {
        let schema = self.lookup_schema(descriptor.id().as_str(), descriptor.version().value())?;
        validate_schema(schema.descriptor(), descriptor)?;
        Ok(schema)
    }

    pub fn encode(&self, envelope: &ChangeEnvelope) -> Result<Bytes, EnvelopeCodecError> {
        self.schema(envelope.changes().schema())?;
        let mut lineage = Vec::new();
        let mut parent = envelope.lineage().cloned();
        while let Some(entry) = parent {
            lineage.push((
                Identity::from_envelope(entry.envelope_id()),
                Metadata::from(entry.system().as_ref()),
            ));
            parent = entry.parent().cloned();
        }
        let mut annotations: Vec<_> = envelope
            .annotations()
            .entries()
            .map(Annotation::from)
            .collect();
        annotations.reverse();
        let frame = Frame {
            format: 1,
            id: Identity::from_envelope(envelope.id()),
            change_set: Identity {
                namespace: envelope.changes().id().namespace().to_owned(),
                value: envelope.changes().id().value().to_vec(),
            },
            schema: Descriptor::from(envelope.changes().schema()),
            operations: envelope
                .changes()
                .operations()
                .iter()
                .map(Operation::from)
                .collect(),
            system: Metadata::from(envelope.system().as_ref()),
            lineage,
            context_identity: envelope.context_identity(),
            annotations,
        };
        let mut output = LimitedBuffer {
            bytes: Vec::new(),
            limit: self.max_bytes.get(),
            exceeded: false,
        };
        let encoded = serde_json::to_writer(&mut output, &frame);
        if output.exceeded {
            return Err(EnvelopeCodecError::SizeLimit {
                limit: self.max_bytes.get(),
            });
        }
        encoded?;
        Ok(Bytes::from(output.bytes))
    }

    pub fn decode(&self, bytes: &[u8]) -> Result<ChangeEnvelope, EnvelopeCodecError> {
        if bytes.len() > self.max_bytes.get() {
            return Err(EnvelopeCodecError::SizeLimit {
                limit: self.max_bytes.get(),
            });
        }
        let frame: Frame = serde_json::from_slice(bytes)?;
        if frame.format != 1 {
            return Err(EnvelopeCodecError::Version(frame.format));
        }
        let descriptor = frame.schema.decode()?;
        let schema = self.schema(&descriptor)?;
        let operations = frame
            .operations
            .into_iter()
            .map(|operation| operation.decode(schema))
            .collect::<Result<_, _>>()?;
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                frame.change_set.namespace,
                Bytes::from(frame.change_set.value),
            )?,
            descriptor,
            operations,
        )?;
        let mut lineage = None;
        for (id, metadata) in frame.lineage.into_iter().rev() {
            lineage = Some(Lineage::restore(
                id.envelope()?,
                metadata.decode()?,
                lineage,
            ));
        }
        let annotations = frame
            .annotations
            .into_iter()
            .map(Annotation::decode)
            .collect::<Result<_, _>>()?;
        Ok(ChangeEnvelope::restore(
            frame.id.envelope()?,
            changes,
            frame.system.decode()?,
            lineage,
            frame.context_identity,
            annotations,
        )?)
    }
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl std::io::Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "computation envelope storage size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    namespace: String,
    value: Vec<u8>,
}

impl Identity {
    fn from_envelope(id: &EnvelopeId) -> Self {
        Self {
            namespace: id.namespace().to_owned(),
            value: id.value().to_vec(),
        }
    }
    fn from_record(id: &RecordId) -> Self {
        Self {
            namespace: id.namespace().to_owned(),
            value: id.value().to_vec(),
        }
    }
    fn envelope(self) -> Result<EnvelopeId, ContractError> {
        EnvelopeId::try_new(self.namespace, Bytes::from(self.value))
    }
    fn record(self) -> Result<RecordId, ContractError> {
        RecordId::try_new(self.namespace, Bytes::from(self.value))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    id: String,
    version: u32,
    encoding: String,
    definition: Vec<u8>,
}

impl From<&SchemaDescriptor> for Descriptor {
    fn from(value: &SchemaDescriptor) -> Self {
        Self {
            id: value.id().as_str().to_owned(),
            version: value.version().value(),
            encoding: value.encoding().to_owned(),
            definition: value.definition().to_vec(),
        }
    }
}

impl Descriptor {
    fn decode(self) -> Result<SchemaDescriptor, ContractError> {
        SchemaDescriptor::try_new(
            SchemaId::try_new(self.id)?,
            SchemaVersion::try_new(self.version)?,
            self.encoding,
            Bytes::from(self.definition),
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    stream: String,
    sequence: u64,
    timestamp: Option<DateTime<Utc>>,
    source_position: Option<Vec<u8>>,
    control: Option<Control>,
    ordering: ChangeOrdering,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    kind: String,
    payload: Vec<u8>,
}

impl From<&SystemMetadata> for Metadata {
    fn from(value: &SystemMetadata) -> Self {
        Self {
            stream: value.stream().as_str().to_owned(),
            sequence: value.sequence(),
            timestamp: value.timestamp(),
            source_position: value.source_position().map(|value| value.to_vec()),
            control: value.control().map(|control| Control {
                kind: control.kind().to_owned(),
                payload: control.payload().to_vec(),
            }),
            ordering: value.ordering(),
        }
    }
}

impl Metadata {
    fn decode(self) -> Result<SystemMetadata, ContractError> {
        let mut value = SystemMetadata::new(StreamId::try_new(self.stream)?, self.sequence)
            .with_ordering(self.ordering);
        if let Some(timestamp) = self.timestamp {
            value = value.with_timestamp(timestamp);
        }
        if let Some(position) = self.source_position {
            value = value.with_source_position(Bytes::from(position));
        }
        if let Some(control) = self.control {
            value = value.with_control(ControlMessage::try_new(
                control.kind,
                Bytes::from(control.payload),
            )?);
        }
        Ok(value)
    }
}

#[derive(Serialize, Deserialize)]
enum ImageKind {
    Full,
    Patch,
    Partial,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    identity: Identity,
    kind: ImageKind,
    bytes: Vec<u8>,
}

impl From<&Record> for Image {
    fn from(value: &Record) -> Self {
        Self {
            identity: Identity::from_record(value.identity()),
            kind: match value.image() {
                RecordImage::Full => ImageKind::Full,
                RecordImage::Patch => ImageKind::Patch,
                RecordImage::Partial => ImageKind::Partial,
            },
            bytes: value.payload().to_vec(),
        }
    }
}

impl Image {
    fn decode(self, schema: &Schema) -> Result<Record, ContractError> {
        Record::try_new(
            schema,
            self.identity.record()?,
            match self.kind {
                ImageKind::Full => RecordImage::Full,
                ImageKind::Patch => RecordImage::Patch,
                ImageKind::Partial => RecordImage::Partial,
            },
            Bytes::from(self.bytes),
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Operation {
    Add {
        ordinal: u64,
        after: Image,
    },
    Update {
        ordinal: u64,
        before: Option<Image>,
        after: Image,
        replace: bool,
    },
    Delete {
        ordinal: u64,
        identity: Identity,
        before: Option<Image>,
    },
}

impl From<&ChangeOperation> for Operation {
    fn from(value: &ChangeOperation) -> Self {
        match value {
            ChangeOperation::Added { ordinal, after } => Self::Add {
                ordinal: *ordinal,
                after: after.into(),
            },
            ChangeOperation::Updated {
                ordinal,
                before,
                after,
                semantics,
            } => Self::Update {
                ordinal: *ordinal,
                before: before.as_ref().map(Image::from),
                after: after.into(),
                replace: *semantics == UpdateSemantics::Replace,
            },
            ChangeOperation::Deleted {
                ordinal,
                identity,
                before,
            } => Self::Delete {
                ordinal: *ordinal,
                identity: Identity::from_record(identity.identity()),
                before: before.as_ref().map(Image::from),
            },
        }
    }
}

impl Operation {
    fn decode(self, schema: &Schema) -> Result<ChangeOperation, ContractError> {
        Ok(match self {
            Self::Add { ordinal, after } => ChangeOperation::Added {
                ordinal,
                after: after.decode(schema)?,
            },
            Self::Update {
                ordinal,
                before,
                after,
                replace,
            } => ChangeOperation::Updated {
                ordinal,
                before: before.map(|image| image.decode(schema)).transpose()?,
                after: after.decode(schema)?,
                semantics: if replace {
                    UpdateSemantics::Replace
                } else {
                    UpdateSemantics::Patch
                },
            },
            Self::Delete {
                ordinal,
                identity,
                before,
            } => ChangeOperation::Deleted {
                ordinal,
                identity: RecordReference::try_new(schema, identity.record()?)?,
                before: before.map(|image| image.decode(schema)).transpose()?,
            },
        })
    }
}

#[derive(Serialize, Deserialize)]
enum Value {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    String(String),
    Bytes(Vec<u8>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Annotation {
    contributor: String,
    key: String,
    value: Value,
}

impl From<ContextEntry> for Annotation {
    fn from(entry: ContextEntry) -> Self {
        Self {
            contributor: entry.contributor().to_owned(),
            key: entry.key().to_owned(),
            value: match entry.value() {
                ContextValue::Bool(value) => Value::Bool(value),
                ContextValue::Signed(value) => Value::Signed(value),
                ContextValue::Unsigned(value) => Value::Unsigned(value),
                ContextValue::String(value) => Value::String(value.to_string()),
                ContextValue::Bytes(value) => Value::Bytes(value.to_vec()),
            },
        }
    }
}

impl Annotation {
    fn decode(self) -> Result<ContextEntry, ContractError> {
        ContextEntry::try_new(
            ComponentId::try_new(self.contributor)?,
            self.key,
            match self.value {
                Value::Bool(value) => ContextValue::Bool(value),
                Value::Signed(value) => ContextValue::Signed(value),
                Value::Unsigned(value) => ContextValue::Unsigned(value),
                Value::String(value) => ContextValue::String(Arc::from(value)),
                Value::Bytes(value) => ContextValue::Bytes(Arc::from(value)),
            },
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    format: u32,
    id: Identity,
    change_set: Identity,
    schema: Descriptor,
    operations: Vec<Operation>,
    system: Metadata,
    lineage: Vec<(Identity, Metadata)>,
    context_identity: u64,
    annotations: Vec<Annotation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RecordValidationError, RecordValidator};

    struct NoRecords;

    impl RecordValidator for NoRecords {
        fn validate_identity(
            &self,
            _: &SchemaDescriptor,
            _: &RecordId,
        ) -> Result<(), crate::RecordValidationError> {
            Err(RecordValidationError::new("identity", "no records"))
        }

        fn validate(
            &self,
            schema: &SchemaDescriptor,
            id: &RecordId,
            _: RecordImage,
            _: &[u8],
        ) -> Result<(), crate::RecordValidationError> {
            self.validate_identity(schema, id)
        }
    }

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("test.control").unwrap(),
                SchemaVersion::try_new(1).unwrap(),
                "none",
                Bytes::from_static(b"empty"),
            )
            .unwrap(),
            Arc::new(NoRecords),
        ))
    }

    fn codec(limit: usize) -> EnvelopeCodec {
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(limit).unwrap());
        codec.register_schema(schema()).unwrap();
        codec
    }

    fn metadata(stream: &str, sequence: u64) -> SystemMetadata {
        SystemMetadata::new(StreamId::try_new(stream).unwrap(), sequence)
    }

    fn control(kind: &str, payload: &[u8]) -> ControlMessage {
        ControlMessage::try_new(kind, Bytes::copy_from_slice(payload)).unwrap()
    }

    fn envelope(
        current: Option<ControlMessage>,
        historical: Option<ControlMessage>,
    ) -> ChangeEnvelope {
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new("changes", Bytes::from_static(b"empty")).unwrap(),
            schema().descriptor().clone(),
            Vec::new(),
        )
        .unwrap();
        let mut source = metadata("source", 1);
        if let Some(control) = historical {
            source = source.with_control(control);
        }
        let input = ChangeEnvelope::new(
            EnvelopeId::try_new("source", Bytes::from_static(b"event")).unwrap(),
            changes.clone(),
            source,
        );
        let mut output = metadata("output", 2);
        if let Some(control) = current {
            output = output.with_control(control);
        }
        input.derive(
            EnvelopeId::try_new("output", Bytes::from_static(b"event")).unwrap(),
            changes,
            output,
        )
    }

    #[test]
    fn json_roundtrip_preserves_current_and_historical_controls_exactly() {
        let codec = codec(4096);
        let payloads: [&[u8]; 3] = [&[], b"payload", &[255, 0, 128, 10]];
        for payload in payloads {
            let current = control("custom/reevaluate", payload);
            let historical = control("custom/history", &[9, 0, 255]);
            let original = envelope(Some(current.clone()), Some(historical.clone()));
            let encoded = codec.encode(&original).unwrap();
            let decoded = codec.decode(&encoded).unwrap();
            assert_eq!(decoded.system().control(), Some(&current));
            assert_eq!(
                decoded.lineage().unwrap().system().control(),
                Some(&historical)
            );
            assert_eq!(codec.encode(&decoded).unwrap(), encoded);
        }
        let encoded = codec
            .encode(&envelope(None, Some(control("custom/history", &[]))))
            .unwrap();
        let decoded = codec.decode(&encoded).unwrap();
        assert!(decoded.system().control().is_none());
        assert_eq!(
            decoded
                .lineage()
                .unwrap()
                .system()
                .control()
                .unwrap()
                .kind(),
            "custom/history"
        );
        let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(value["format"], 1);
        assert!(value["system"]["control"].is_null());
    }

    #[test]
    fn json_decode_rejects_invalid_control_kinds_and_unknown_control_fields() {
        let codec = codec(4096);
        let encoded = codec
            .encode(&envelope(
                Some(control("custom/current", b"c")),
                Some(control("custom/history", b"h")),
            ))
            .unwrap();
        let original: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        for kind in ["", "has space", "line\nbreak", "nul\u{0}"] {
            for lineage in [false, true] {
                let mut value = original.clone();
                let target = if lineage {
                    &mut value["lineage"][0][1]["control"]["kind"]
                } else {
                    &mut value["system"]["control"]["kind"]
                };
                *target = kind.into();
                let changed = serde_json::to_vec(&value).unwrap();
                assert!(
                    matches!(
                        codec.decode(&changed),
                        Err(EnvelopeCodecError::Contract(
                            ContractError::InvalidIdentifier { .. }
                        ))
                    ),
                    "{kind:?} lineage={lineage}"
                );
            }
        }
        let mut value = original;
        value["system"]["control"]["extra"] = true.into();
        assert!(codec.decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn json_roundtrip_preserves_ordering_in_current_and_lineage_metadata() {
        let codec = codec(4096);
        for (current, historical) in [
            (ChangeOrdering::EventTime, ChangeOrdering::ProducerSequence),
            (ChangeOrdering::ProducerSequence, ChangeOrdering::EventTime),
        ] {
            let changes = ChangeSet::try_new(
                ChangeSetId::try_new("changes", Bytes::from_static(b"empty")).unwrap(),
                schema().descriptor().clone(),
                Vec::new(),
            )
            .unwrap();
            let input = ChangeEnvelope::new(
                EnvelopeId::try_new("source", Bytes::from_static(b"event")).unwrap(),
                changes.clone(),
                metadata("source", 1).with_ordering(historical),
            );
            let original = input.derive(
                EnvelopeId::try_new("output", Bytes::from_static(b"event")).unwrap(),
                changes,
                metadata("output", 2).with_ordering(current),
            );
            let encoded = codec.encode(&original).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            let name = |ordering| serde_json::to_value(ordering).unwrap();
            assert_eq!(value["format"], 1);
            assert_eq!(value["system"]["ordering"], name(current));
            assert_eq!(value["lineage"][0][1]["ordering"], name(historical));
            let decoded = codec.decode(&encoded).unwrap();
            assert_eq!(decoded.system().ordering(), current);
            assert_eq!(decoded.lineage().unwrap().system().ordering(), historical);
            assert_eq!(codec.encode(&decoded).unwrap(), encoded);
            let replay = decoded.reemit(3).unwrap();
            assert_eq!(replay.system().ordering(), current);
            assert_eq!(replay.lineage().unwrap().system().ordering(), historical);
        }
        let default = codec.encode(&envelope(None, None)).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&default).unwrap();
        assert_eq!(value["system"]["ordering"], "EventTime");
        assert_eq!(value["lineage"][0][1]["ordering"], "EventTime");
    }

    #[test]
    fn json_decode_rejects_invalid_or_missing_ordering() {
        let codec = codec(4096);
        let original: serde_json::Value =
            serde_json::from_slice(&codec.encode(&envelope(None, None)).unwrap()).unwrap();
        for lineage in [false, true] {
            for invalid in [
                serde_json::json!("Sequence"),
                serde_json::json!("eventtime"),
                serde_json::json!(0),
                serde_json::json!(null),
                serde_json::json!({"EventTime": null, "ProducerSequence": null}),
            ] {
                let mut value = original.clone();
                let target = if lineage {
                    &mut value["lineage"][0][1]
                } else {
                    &mut value["system"]
                };
                target["ordering"] = invalid.clone();
                assert!(
                    codec.decode(&serde_json::to_vec(&value).unwrap()).is_err(),
                    "{invalid} lineage={lineage}"
                );
            }
            let mut value = original.clone();
            let target = if lineage {
                &mut value["lineage"][0][1]
            } else {
                &mut value["system"]
            };
            target.as_object_mut().unwrap().remove("ordering");
            assert!(
                codec.decode(&serde_json::to_vec(&value).unwrap()).is_err(),
                "missing lineage={lineage}"
            );
        }
    }

    #[test]
    fn json_size_limit_includes_current_and_historical_controls() {
        let large = vec![255; 512];
        let with_controls = envelope(
            Some(control("custom/current", &large)),
            Some(control("custom/history", &large)),
        );
        let encoded = codec(1 << 20).encode(&with_controls).unwrap();
        assert!(codec(encoded.len()).encode(&with_controls).is_ok());
        assert!(matches!(
            codec(encoded.len() - 1).encode(&with_controls),
            Err(EnvelopeCodecError::SizeLimit { .. })
        ));
        assert!(encoded.len() > codec(1 << 20).encode(&envelope(None, None)).unwrap().len() + 1024);
    }
}
