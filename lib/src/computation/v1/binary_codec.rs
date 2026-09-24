// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Versioned binary transport, independent of the persisted JSON EnvelopeCodec.

use std::{borrow::Cow, io::Write, num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId, ComponentId, ContextEntry,
    ContextValue, ContractError, EnvelopeCodec, EnvelopeCodecError, EnvelopeId, Lineage, Record,
    RecordId, RecordImage, RecordReference, Schema, SchemaDescriptor, StreamId, SystemMetadata,
    UpdateSemantics,
};

#[derive(Debug, thiserror::Error)]
pub enum BinaryEnvelopeCodecError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Schema(#[from] EnvelopeCodecError),
    #[error(transparent)]
    Encoding(#[from] rmp_serde::encode::Error),
    #[error(transparent)]
    Decoding(#[from] rmp_serde::decode::Error),
    #[error("unsupported binary computation envelope version {0}")]
    Version(u32),
    #[error("binary computation message exceeds the configured {limit}-byte limit")]
    SizeLimit { limit: usize },
    #[error("invalid binary computation message: {0}")]
    InvalidFrame(&'static str),
}

type Result<T> = std::result::Result<T, BinaryEnvelopeCodecError>;

/// Full-fidelity native transport. This does not read or write persisted JSON.
pub struct BinaryEnvelopeCodec {
    registry: EnvelopeCodec,
    max_bytes: NonZeroUsize,
}

impl BinaryEnvelopeCodec {
    pub const VERSION: u32 = 2;

    pub fn new(max_bytes: NonZeroUsize) -> Self {
        Self {
            registry: EnvelopeCodec::new(max_bytes),
            max_bytes,
        }
    }

    pub fn register_schema(&mut self, schema: Arc<Schema>) -> Result<()> {
        self.registry.register_schema(schema)?;
        Ok(())
    }

    pub fn encode(&self, envelope: &ChangeEnvelope) -> Result<Vec<u8>> {
        self.registry.schema(envelope.changes().schema())?;
        let mut lineage = Vec::new();
        let mut parent = envelope.lineage();
        while let Some(entry) = parent {
            lineage.push((
                Identity::envelope(entry.envelope_id()),
                Metadata::from(entry.system().as_ref()),
            ));
            parent = entry.parent();
        }
        let entries: Vec<_> = envelope.annotations().entries().collect();
        let frame = Frame {
            format: Self::VERSION,
            id: Identity::envelope(envelope.id()),
            change_set: Identity {
                namespace: Cow::Borrowed(envelope.changes().id().namespace()),
                value: Buffer::borrowed(envelope.changes().id().value()),
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
            annotations: entries.iter().map(Annotation::from).collect(),
        };
        encode_bounded_messagepack(&frame, self.max_bytes.get())
    }

    pub fn decode(&self, bytes: &[u8]) -> Result<ChangeEnvelope> {
        let frame: Frame<'_> = decode_bounded_messagepack(bytes, self.max_bytes.get())?;
        if frame.format != Self::VERSION {
            return Err(BinaryEnvelopeCodecError::Version(frame.format));
        }
        let schema = self
            .registry
            .lookup_schema(&frame.schema.id, frame.schema.version)?;
        if frame.schema.encoding != schema.descriptor().encoding()
            || frame.schema.definition.0.as_ref() != schema.descriptor().definition().as_ref()
        {
            return Err(BinaryEnvelopeCodecError::InvalidFrame(
                "registered schema definition mismatch",
            ));
        }
        let operations = frame
            .operations
            .into_iter()
            .map(|operation| operation.decode(schema))
            .collect::<Result<Vec<_>>>()?;
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                frame.change_set.namespace.as_ref(),
                frame.change_set.value.owned(),
            )?,
            schema.descriptor().clone(),
            operations,
        )?;
        let mut lineage = None;
        for (id, metadata) in frame.lineage.into_iter().rev() {
            lineage = Some(Lineage::restore(
                id.into_envelope()?,
                metadata.decode()?,
                lineage,
            ));
        }
        let annotations = frame
            .annotations
            .into_iter()
            .rev()
            .map(Annotation::decode)
            .collect::<Result<Vec<_>>>()?;
        Ok(ChangeEnvelope::restore(
            frame.id.into_envelope()?,
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

impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "binary computation message size limit",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encode a single named MessagePack value without growing past its byte limit.
#[doc(hidden)]
pub fn encode_bounded_messagepack<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    let mut output = LimitedBuffer {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    let encoded = rmp_serde::encode::write_named(&mut output, value);
    if output.exceeded {
        return Err(BinaryEnvelopeCodecError::SizeLimit { limit });
    }
    encoded?;
    Ok(output.bytes)
}

/// Borrow opaque buffers during decoding; returned borrows cannot outlive input.
#[doc(hidden)]
pub fn decode_bounded_messagepack<'de, T: Deserialize<'de>>(
    bytes: &'de [u8],
    limit: usize,
) -> Result<T> {
    if bytes.len() > limit {
        return Err(BinaryEnvelopeCodecError::SizeLimit { limit });
    }
    validate_frame(bytes)?;
    Ok(rmp_serde::from_slice(bytes)?)
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    if len > input.len() {
        return Err(BinaryEnvelopeCodecError::InvalidFrame("truncated value"));
    }
    let (value, rest) = input.split_at(len);
    *input = rest;
    Ok(value)
}

fn length(input: &mut &[u8], width: usize) -> Result<usize> {
    Ok(take(input, width)?
        .iter()
        .fold(0usize, |value, byte| (value << 8) | usize::from(*byte)))
}

// Check framing before serde can reserve from a hostile collection length.
// Binary/string bodies are skipped, not visited byte by byte.
fn validate_frame(mut input: &[u8]) -> Result<()> {
    let mut remaining = vec![1usize];
    while let Some(count) = remaining.last_mut() {
        if *count == 0 {
            remaining.pop();
            continue;
        }
        *count -= 1;
        let marker = take(&mut input, 1)?[0];
        let mut children = 0usize;
        let body = match marker {
            0x00..=0x7f | 0xc0 | 0xc2 | 0xc3 | 0xe0..=0xff => 0,
            0x80..=0x8f => {
                children = usize::from(marker & 15) * 2;
                0
            }
            0x90..=0x9f => {
                children = usize::from(marker & 15);
                0
            }
            0xa0..=0xbf => usize::from(marker & 31),
            0xc1 => {
                return Err(BinaryEnvelopeCodecError::InvalidFrame(
                    "reserved MessagePack marker",
                ))
            }
            0xc4 | 0xd9 => length(&mut input, 1)?,
            0xc5 | 0xda => length(&mut input, 2)?,
            0xc6 | 0xdb => length(&mut input, 4)?,
            0xc7..=0xc9 => length(&mut input, 1usize << (marker - 0xc7))?
                .checked_add(1)
                .ok_or(BinaryEnvelopeCodecError::InvalidFrame(
                    "extension length overflow",
                ))?,
            0xca | 0xce | 0xd2 => 4,
            0xcb | 0xcf | 0xd3 => 8,
            0xcc | 0xd0 => 1,
            0xcd | 0xd1 => 2,
            0xd4..=0xd8 => (1usize << (marker - 0xd4)) + 1,
            0xdc | 0xdd => {
                children = length(&mut input, if marker == 0xdc { 2 } else { 4 })?;
                0
            }
            0xde | 0xdf => {
                children = length(&mut input, if marker == 0xde { 2 } else { 4 })?
                    .checked_mul(2)
                    .ok_or(BinaryEnvelopeCodecError::InvalidFrame(
                        "map length overflow",
                    ))?;
                0
            }
        };
        take(&mut input, body)?;
        if children > input.len() {
            return Err(BinaryEnvelopeCodecError::InvalidFrame(
                "collection length exceeds remaining input",
            ));
        }
        if children != 0 {
            if remaining.len() >= 1024 {
                return Err(BinaryEnvelopeCodecError::InvalidFrame(
                    "MessagePack nesting limit",
                ));
            }
            remaining.push(children);
        }
    }
    if !input.is_empty() {
        return Err(BinaryEnvelopeCodecError::InvalidFrame("trailing bytes"));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct Buffer<'a>(#[serde(borrow, with = "serde_bytes")] Cow<'a, [u8]>);

impl<'a> Buffer<'a> {
    fn borrowed(bytes: &'a [u8]) -> Self {
        Self(Cow::Borrowed(bytes))
    }

    fn owned(self) -> Bytes {
        match self.0 {
            Cow::Borrowed(bytes) => Bytes::copy_from_slice(bytes),
            Cow::Owned(bytes) => Bytes::from(bytes),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity<'a> {
    #[serde(borrow)]
    namespace: Cow<'a, str>,
    #[serde(borrow)]
    value: Buffer<'a>,
}

impl<'a> Identity<'a> {
    fn envelope(id: &'a EnvelopeId) -> Self {
        Self {
            namespace: Cow::Borrowed(id.namespace()),
            value: Buffer::borrowed(id.value()),
        }
    }
    fn record(id: &'a RecordId) -> Self {
        Self {
            namespace: Cow::Borrowed(id.namespace()),
            value: Buffer::borrowed(id.value()),
        }
    }
    fn into_envelope(self) -> Result<EnvelopeId> {
        Ok(EnvelopeId::try_new(
            self.namespace.as_ref(),
            self.value.owned(),
        )?)
    }
    fn into_record(self) -> Result<RecordId> {
        Ok(RecordId::try_new(
            self.namespace.as_ref(),
            self.value.owned(),
        )?)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    version: u32,
    #[serde(borrow)]
    encoding: Cow<'a, str>,
    #[serde(borrow)]
    definition: Buffer<'a>,
}

impl<'a> From<&'a SchemaDescriptor> for Descriptor<'a> {
    fn from(value: &'a SchemaDescriptor) -> Self {
        Self {
            id: Cow::Borrowed(value.id().as_str()),
            version: value.version().value(),
            encoding: Cow::Borrowed(value.encoding()),
            definition: Buffer::borrowed(value.definition()),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata<'a> {
    #[serde(borrow)]
    stream: Cow<'a, str>,
    sequence: u64,
    timestamp: Option<DateTime<Utc>>,
    #[serde(borrow)]
    source_position: Option<Buffer<'a>>,
}

impl<'a> From<&'a SystemMetadata> for Metadata<'a> {
    fn from(value: &'a SystemMetadata) -> Self {
        Self {
            stream: Cow::Borrowed(value.stream().as_str()),
            sequence: value.sequence(),
            timestamp: value.timestamp(),
            source_position: value
                .source_position()
                .map(|position| Buffer::borrowed(position)),
        }
    }
}

impl Metadata<'_> {
    fn decode(self) -> Result<SystemMetadata> {
        let mut metadata =
            SystemMetadata::new(StreamId::try_new(self.stream.as_ref())?, self.sequence);
        if let Some(timestamp) = self.timestamp {
            metadata = metadata.with_timestamp(timestamp);
        }
        if let Some(position) = self.source_position {
            metadata = metadata.with_source_position(position.owned());
        }
        Ok(metadata)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image<'a> {
    #[serde(borrow)]
    identity: Identity<'a>,
    kind: u8,
    #[serde(borrow)]
    bytes: Buffer<'a>,
}

impl<'a> From<&'a Record> for Image<'a> {
    fn from(value: &'a Record) -> Self {
        Self {
            identity: Identity::record(value.identity()),
            kind: match value.image() {
                RecordImage::Full => 0,
                RecordImage::Patch => 1,
                RecordImage::Partial => 2,
            },
            bytes: Buffer::borrowed(value.payload()),
        }
    }
}

impl Image<'_> {
    fn decode(self, schema: &Schema) -> Result<Record> {
        let image = match self.kind {
            0 => RecordImage::Full,
            1 => RecordImage::Patch,
            2 => RecordImage::Partial,
            _ => {
                return Err(BinaryEnvelopeCodecError::InvalidFrame(
                    "unknown record image",
                ))
            }
        };
        Ok(Record::try_new(
            schema,
            self.identity.into_record()?,
            image,
            self.bytes.owned(),
        )?)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Operation<'a> {
    Add {
        ordinal: u64,
        #[serde(borrow)]
        after: Image<'a>,
    },
    Update {
        ordinal: u64,
        #[serde(borrow)]
        before: Option<Image<'a>>,
        #[serde(borrow)]
        after: Image<'a>,
        replace: bool,
    },
    Delete {
        ordinal: u64,
        #[serde(borrow)]
        identity: Identity<'a>,
        #[serde(borrow)]
        before: Option<Image<'a>>,
    },
}

impl<'a> From<&'a ChangeOperation> for Operation<'a> {
    fn from(value: &'a ChangeOperation) -> Self {
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
                identity: Identity::record(identity.identity()),
                before: before.as_ref().map(Image::from),
            },
        }
    }
}

impl Operation<'_> {
    fn decode(self, schema: &Schema) -> Result<ChangeOperation> {
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
                identity: RecordReference::try_new(schema, identity.into_record()?)?,
                before: before.map(|image| image.decode(schema)).transpose()?,
            },
        })
    }
}

#[derive(Serialize, Deserialize)]
enum Value<'a> {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    String(#[serde(borrow)] Cow<'a, str>),
    Bytes(#[serde(borrow)] Buffer<'a>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Annotation<'a> {
    #[serde(borrow)]
    contributor: Cow<'a, str>,
    #[serde(borrow)]
    key: Cow<'a, str>,
    #[serde(borrow)]
    value: Value<'a>,
}

impl<'a> From<&'a ContextEntry> for Annotation<'a> {
    fn from(entry: &'a ContextEntry) -> Self {
        Self {
            contributor: Cow::Borrowed(entry.contributor()),
            key: Cow::Borrowed(entry.key()),
            value: match entry.value_ref() {
                ContextValue::Bool(value) => Value::Bool(*value),
                ContextValue::Signed(value) => Value::Signed(*value),
                ContextValue::Unsigned(value) => Value::Unsigned(*value),
                ContextValue::String(value) => Value::String(Cow::Borrowed(value)),
                ContextValue::Bytes(value) => Value::Bytes(Buffer::borrowed(value)),
            },
        }
    }
}

impl Annotation<'_> {
    fn decode(self) -> Result<ContextEntry> {
        Ok(ContextEntry::try_new(
            ComponentId::try_new(self.contributor.as_ref())?,
            self.key.as_ref(),
            match self.value {
                Value::Bool(value) => ContextValue::Bool(value),
                Value::Signed(value) => ContextValue::Signed(value),
                Value::Unsigned(value) => ContextValue::Unsigned(value),
                Value::String(value) => ContextValue::String(Arc::from(value.as_ref())),
                Value::Bytes(value) => ContextValue::Bytes(Arc::from(value.0.as_ref())),
            },
        )?)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame<'a> {
    format: u32,
    #[serde(borrow)]
    id: Identity<'a>,
    #[serde(borrow)]
    change_set: Identity<'a>,
    #[serde(borrow)]
    schema: Descriptor<'a>,
    #[serde(borrow)]
    operations: Vec<Operation<'a>>,
    #[serde(borrow)]
    system: Metadata<'a>,
    #[serde(borrow)]
    lineage: Vec<(Identity<'a>, Metadata<'a>)>,
    context_identity: u64,
    #[serde(borrow)]
    annotations: Vec<Annotation<'a>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::GraphChangeCodec;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };

    fn codec(limit: usize) -> BinaryEnvelopeCodec {
        let mut codec = BinaryEnvelopeCodec::new(NonZeroUsize::new(limit).unwrap());
        codec.register_schema(GraphChangeCodec::schema()).unwrap();
        codec
    }

    fn envelope() -> ChangeEnvelope {
        let changes = [1, 2].map(|value| SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &format!("node-{value}")),
                    labels: Arc::from([Arc::from("Node")]),
                    effective_from: 1234,
                },
                properties: ElementPropertyMap::new(),
            },
        });
        let source = GraphChangeCodec::encode_changes(
            &changes,
            StreamId::try_new("source").unwrap(),
            7,
            Some(DateTime::from_timestamp(-123, 987654321).unwrap()),
        )
        .unwrap();
        let first = source.derive(
            EnvelopeId::try_new("opaque", Bytes::from_static(&[255, 0, 128])).unwrap(),
            source.changes().clone(),
            SystemMetadata::new(StreamId::try_new("first").unwrap(), u64::MAX)
                .with_source_position(Bytes::from_static(&[255, 0, 128])),
        );
        let mut result = first.derive(
            EnvelopeId::try_new("second", Bytes::from_static(b"derived")).unwrap(),
            first.changes().clone(),
            SystemMetadata::new(StreamId::try_new("second").unwrap(), 1),
        );
        for value in [
            ContextValue::Bool(false),
            ContextValue::Signed(i64::MIN),
            ContextValue::Unsigned(u64::MAX),
            ContextValue::String(Arc::from("unicode \u{2603}")),
            ContextValue::Bytes(Arc::from([255, 0, 128])),
        ] {
            result
                .append_annotation(
                    ContextEntry::try_new(
                        ComponentId::try_new("transform").unwrap(),
                        "same-key",
                        value,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        result
    }

    #[test]
    fn binary_roundtrip_preserves_the_entire_storage_projection_and_branch_isolation() {
        let codec = codec(32_768);
        let original = envelope();
        let encoded = codec.encode(&original).unwrap();
        let decoded = codec.decode(&encoded).unwrap();
        let mut storage = EnvelopeCodec::new(NonZeroUsize::new(32_768).unwrap());
        storage.register_schema(GraphChangeCodec::schema()).unwrap();
        let persisted = storage.encode(&original).unwrap();
        assert_eq!(
            storage.encode(&decoded).unwrap(),
            persisted,
            "records, system metadata, lineage, annotation order and context identity"
        );
        assert_eq!(codec.encode(&decoded).unwrap(), encoded);
        assert!(storage.decode(&encoded).is_err());
        assert!(codec.decode(&persisted).is_err());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&persisted).unwrap()["format"],
            1
        );
        let mut branch = decoded.clone();
        branch
            .append_annotation(
                ContextEntry::try_new(
                    ComponentId::try_new("branch").unwrap(),
                    "only-here",
                    ContextValue::Bool(true),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(codec.encode(&decoded).unwrap(), encoded);
        assert_eq!(branch.annotations().len(), decoded.annotations().len() + 1);
    }

    #[test]
    fn wire_bytes_borrow_the_frame_and_use_bulk_binary_not_integer_arrays() {
        let codec = codec(256_000);
        let encoded = codec.encode(&envelope()).unwrap();
        let mut frame: Frame<'_> = decode_bounded_messagepack(&encoded, 256_000).unwrap();
        assert!(matches!(frame.id.value.0, Cow::Borrowed(_)));
        assert!(matches!(frame.schema.definition.0, Cow::Borrowed(_)));
        let Operation::Add { after, .. } = &frame.operations[0] else {
            unreachable!()
        };
        assert!(matches!(after.bytes.0, Cow::Borrowed(_)));
        assert!(matches!(
            frame.annotations[0].value,
            Value::Bytes(Buffer(Cow::Borrowed(_)))
        ));
        frame.annotations[0].value = Value::Bytes(Buffer::borrowed(&[]));
        let empty = encode_bounded_messagepack(&frame, 256_000).unwrap();
        let bytes = vec![255; 65_536];
        frame.annotations[0].value = Value::Bytes(Buffer::borrowed(&bytes));
        let full = encode_bounded_messagepack(&frame, 256_000).unwrap();
        assert_eq!(
            full.len() - empty.len(),
            bytes.len() + 3,
            "bin32 versus bin8 header, with exactly one byte per payload byte"
        );
        assert_eq!(
            codec
                .decode(&full)
                .unwrap()
                .annotations()
                .entries()
                .next()
                .unwrap()
                .value(),
            ContextValue::Bytes(Arc::from(bytes))
        );
    }

    #[test]
    fn exact_limits_truncation_and_trailing_frames_are_rejected() {
        let envelope = envelope();
        let encoded = codec(32_768).encode(&envelope).unwrap();
        assert_eq!(codec(encoded.len()).encode(&envelope).unwrap(), encoded);
        assert!(codec(encoded.len()).decode(&encoded).is_ok());
        assert!(matches!(
            codec(encoded.len() - 1).encode(&envelope),
            Err(BinaryEnvelopeCodecError::SizeLimit { .. })
        ));
        assert!(matches!(
            codec(encoded.len() - 1).decode(&encoded),
            Err(BinaryEnvelopeCodecError::SizeLimit { .. })
        ));
        for len in 0..encoded.len() {
            assert!(
                codec(32_768).decode(&encoded[..len]).is_err(),
                "prefix {len}"
            );
        }
        for suffix in [&[0xc0][..], &[0x91][..], &[0xc6, 0xff][..]] {
            let mut extra = encoded.clone();
            extra.extend_from_slice(suffix);
            assert!(codec(32_768).decode(&extra).is_err());
        }
    }

    #[test]
    fn invalid_fields_still_use_schema_record_and_change_set_validation() {
        let codec = codec(32_768);
        let encoded = codec.encode(&envelope()).unwrap();
        for fault in [
            "version",
            "schema",
            "definition",
            "encoding",
            "identity",
            "image",
            "ordinal",
            "annotation",
            "record",
        ] {
            let mut frame: Frame<'_> = decode_bounded_messagepack(&encoded, 32_768).unwrap();
            match fault {
                "version" => frame.format = 1,
                "schema" => frame.schema.id = Cow::Borrowed("unknown"),
                "definition" => frame.schema.definition = Buffer::borrowed(&[255]),
                "encoding" => frame.schema.encoding = Cow::Borrowed("wrong"),
                "annotation" => frame.annotations[0].contributor = Cow::Borrowed("invalid id"),
                "ordinal" => {
                    let Operation::Add { ordinal: first, .. } = frame.operations[0] else {
                        unreachable!()
                    };
                    let Operation::Add { ordinal, .. } = &mut frame.operations[1] else {
                        unreachable!()
                    };
                    *ordinal = first;
                }
                _ => {
                    let Operation::Add { after, .. } = &mut frame.operations[0] else {
                        unreachable!()
                    };
                    match fault {
                        "identity" => after.identity.value = Buffer::borrowed(&[255]),
                        "image" => after.kind = 99,
                        "record" => after.bytes = Buffer::borrowed(&[255]),
                        _ => unreachable!(),
                    }
                }
            }
            let changed = encode_bounded_messagepack(&frame, 32_768).unwrap();
            assert!(codec.decode(&changed).is_err(), "{fault}");
        }
        for field in ["unknown-field", "format"] {
            let mut changed = encoded.clone();
            assert!((0x80..0x8f).contains(&changed[0]));
            changed[0] += 1;
            changed.extend_from_slice(&encode_bounded_messagepack(&field, 64).unwrap());
            changed.push(2);
            assert!(
                codec.decode(&changed).is_err(),
                "unexpected/duplicate {field}"
            );
        }
    }

    #[test]
    fn framing_bounds_lengths_and_nesting_before_allocating_collections() {
        for bytes in [
            vec![0xc6, 255, 255, 255, 255],
            vec![0xdb, 255, 255, 255, 255],
            vec![0xdd, 255, 255, 255, 255],
            vec![0xdf, 255, 255, 255, 255],
            vec![0xc9, 255, 255, 255, 255],
            vec![0xc1],
        ] {
            assert!(decode_bounded_messagepack::<serde::de::IgnoredAny>(&bytes, 64).is_err());
        }
        let mut nested = vec![0x91; 1024];
        nested.push(0xc0);
        assert!(decode_bounded_messagepack::<serde::de::IgnoredAny>(&nested, 2048).is_err());
        for size in [0, 31, 32, 255, 256, 65_535, 65_536] {
            let value = serde_bytes::ByteBuf::from(vec![128; size]);
            let encoded = encode_bounded_messagepack(&value, size + 5).unwrap();
            let decoded: serde_bytes::ByteBuf =
                decode_bounded_messagepack(&encoded, encoded.len()).unwrap();
            assert_eq!(decoded, value);
            assert!(encode_bounded_messagepack(&value, encoded.len() - 1).is_err());
        }
    }

    #[test]
    fn binary_preserves_replace_patch_partial_images_and_identity_only_deletes() {
        use crate::computation::v1::{
            RecordValidationError, RecordValidator, SchemaId, SchemaVersion,
        };

        struct Validator;
        impl RecordValidator for Validator {
            fn validate_identity(
                &self,
                _: &SchemaDescriptor,
                id: &RecordId,
            ) -> std::result::Result<(), RecordValidationError> {
                if id.namespace() == "rows" && id.value().as_ref() == [1] {
                    Ok(())
                } else {
                    Err(RecordValidationError::new(
                        "identity",
                        "expected rows key 1",
                    ))
                }
            }
            fn validate(
                &self,
                schema: &SchemaDescriptor,
                id: &RecordId,
                _: RecordImage,
                bytes: &[u8],
            ) -> std::result::Result<(), RecordValidationError> {
                self.validate_identity(schema, id)?;
                if bytes.len() == 3 && bytes[0] == 1 {
                    Ok(())
                } else {
                    Err(RecordValidationError::new(
                        "payload",
                        "expected three bytes and matching key",
                    ))
                }
            }
        }
        let schema = Arc::new(Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("test.images").unwrap(),
                SchemaVersion::try_new(1).unwrap(),
                "three-bytes",
                Bytes::from_static(b"key,value;full-patch-partial"),
            )
            .unwrap(),
            Arc::new(Validator),
        ));
        let identity = RecordId::try_new("rows", Bytes::from_static(&[1])).unwrap();
        let record = |image| {
            Record::try_new(
                &schema,
                identity.clone(),
                image,
                Bytes::from_static(&[1, 0, 255]),
            )
            .unwrap()
        };
        let reference = RecordReference::try_new(&schema, identity.clone()).unwrap();
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new("changes", Bytes::from_static(b"all-kinds")).unwrap(),
            schema.descriptor().clone(),
            vec![
                ChangeOperation::Added {
                    ordinal: 1,
                    after: record(RecordImage::Full),
                },
                ChangeOperation::Updated {
                    ordinal: 2,
                    before: Some(record(RecordImage::Partial)),
                    after: record(RecordImage::Full),
                    semantics: UpdateSemantics::Replace,
                },
                ChangeOperation::Updated {
                    ordinal: 3,
                    before: None,
                    after: record(RecordImage::Patch),
                    semantics: UpdateSemantics::Patch,
                },
                ChangeOperation::Deleted {
                    ordinal: 4,
                    identity: reference.clone(),
                    before: Some(record(RecordImage::Full)),
                },
                ChangeOperation::Deleted {
                    ordinal: 5,
                    identity: reference,
                    before: None,
                },
            ],
        )
        .unwrap();
        let original = ChangeEnvelope::new(
            EnvelopeId::try_new("images", Bytes::from_static(b"event")).unwrap(),
            changes,
            SystemMetadata::new(StreamId::try_new("images/out").unwrap(), 1),
        );
        let mut codec = BinaryEnvelopeCodec::new(NonZeroUsize::new(32_768).unwrap());
        codec.register_schema(schema.clone()).unwrap();
        let decoded = codec.decode(&codec.encode(&original).unwrap()).unwrap();
        assert_eq!(
            decoded.changes().operations(),
            original.changes().operations()
        );
        let mut stored = EnvelopeCodec::new(NonZeroUsize::new(32_768).unwrap());
        stored.register_schema(schema).unwrap();
        assert_eq!(
            stored.encode(&decoded).unwrap(),
            stored.encode(&original).unwrap()
        );
    }
}
