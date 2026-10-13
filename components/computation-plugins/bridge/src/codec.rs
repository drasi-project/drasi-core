// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{borrow::Cow, num::NonZeroU64};

use drasi_lib::computation::v1::{
    BinaryEnvelopeCodec, ChangeEnvelope, ChangeOperation, ContextValue, EnvelopeCodec, Record,
    SourceTransactionCodec, SourceTransactionLimits,
};
use serde::Deserialize;

use crate::{
    payload::{annotation_json, PayloadGuard},
    BridgeError, Limits, Namespace, PortableSchema, Result,
};

/// Only the three standard executable schemas are installed. Peer metadata
/// cannot cause discovery or loading of any plugin code.
pub struct PortableEnvelopeCodec {
    binary: BinaryEnvelopeCodec,
    schema: PortableSchema,
    limits: Limits,
    namespace: Namespace,
}

impl PortableEnvelopeCodec {
    pub fn new(schema: PortableSchema, namespace: Namespace, limits: Limits) -> Result<Self> {
        limits.validate()?;
        namespace.validate()?;
        let mut binary = BinaryEnvelopeCodec::new(limits.envelope_limit());
        binary
            .register_schema(schema.schema())
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        Ok(Self {
            binary,
            schema,
            limits,
            namespace,
        })
    }

    pub(crate) fn logical_codec(&self) -> Result<EnvelopeCodec> {
        // The persisted JSON codec is used by the library's canonical identity,
        // never as the bridge wire. Its byte-array representation can be larger.
        let mut codec = EnvelopeCodec::new(
            std::num::NonZeroUsize::new(self.limits.max_envelope_bytes * 8)
                .expect("nonzero canonical envelope limit"),
        );
        codec
            .register_schema(self.schema.schema())
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        Ok(codec)
    }

    pub fn encode(&self, envelope: &ChangeEnvelope) -> Result<Vec<u8>> {
        self.validate(envelope)?;
        self.binary
            .encode(envelope)
            .map_err(|error| BridgeError::invalid(error.to_string()))
    }

    pub fn decode(&self, bytes: &[u8]) -> Result<ChangeEnvelope> {
        preflight_messagepack(
            bytes,
            self.limits.max_envelope_bytes,
            self.limits.max_values,
        )?;
        // This borrowed probe cannot build recursive values or lineage. It checks
        // every opaque record before the normal executable validator sees bytes.
        let probe: EnvelopeProbe<'_> = rmp_serde::from_slice(bytes)
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        probe.validate(self)?;
        let envelope = self
            .binary
            .decode(bytes)
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        self.validate_semantics(&envelope)?;
        Ok(envelope)
    }

    pub fn validate(&self, envelope: &ChangeEnvelope) -> Result<()> {
        if envelope.changes().schema() != self.schema.schema().descriptor()
            || envelope.changes().operations().len() > self.limits.max_operations
            || envelope.annotations().len() > self.limits.max_annotations
        {
            return Err(BridgeError::invalid(
                "envelope schema or collection limit mismatch",
            ));
        }
        identity(envelope.id().namespace(), envelope.id().value())?;
        identity(
            envelope.changes().id().namespace(),
            envelope.changes().id().value(),
        )?;
        crate::config::identifier(envelope.system().stream().as_str())?;
        position(
            envelope
                .system()
                .source_position()
                .map(|value| value.as_ref()),
        )?;
        let mut lineage = envelope.lineage();
        let mut count = 0;
        while let Some(entry) = lineage {
            count += 1;
            if count > self.limits.max_lineage {
                return Err(BridgeError::invalid("envelope lineage limit"));
            }
            identity(entry.envelope_id().namespace(), entry.envelope_id().value())?;
            crate::config::identifier(entry.system().stream().as_str())?;
            position(entry.system().source_position().map(|value| value.as_ref()))?;
            lineage = entry.parent();
        }
        for entry in envelope.annotations().entries() {
            crate::config::identifier(entry.contributor())?;
            crate::config::identifier(entry.key())?;
            let value = entry.value();
            match value {
                ContextValue::String(value) if value.len() > 64 * 1024 => {
                    return Err(BridgeError::invalid("annotation string limit"));
                }
                ContextValue::Bytes(value) => annotation(entry.key(), &value, &self.limits)?,
                _ => {}
            }
        }
        let mut guard = PayloadGuard::new(&self.limits, &self.namespace);
        for operation in envelope.changes().operations() {
            match operation {
                ChangeOperation::Added { after, .. } => self.record(after, &mut guard)?,
                ChangeOperation::Updated { before, after, .. } => {
                    if let Some(before) = before {
                        self.record(before, &mut guard)?;
                    }
                    self.record(after, &mut guard)?;
                }
                ChangeOperation::Deleted {
                    identity, before, ..
                } => {
                    guard.identity(
                        self.schema,
                        identity.identity().namespace(),
                        identity.identity().value(),
                    )?;
                    if let Some(before) = before {
                        self.record(before, &mut guard)?;
                    }
                }
            }
        }
        self.validate_semantics(envelope)
    }

    fn record(&self, record: &Record, guard: &mut PayloadGuard<'_>) -> Result<()> {
        guard.identity(
            self.schema,
            record.identity().namespace(),
            record.identity().value(),
        )?;
        guard.payload(self.schema, record.payload())
    }

    fn validate_semantics(&self, envelope: &ChangeEnvelope) -> Result<()> {
        if envelope.system().sequence() == 0 {
            return Err(BridgeError::invalid(
                "bridge transport sequences start at one",
            ));
        }
        match self.schema {
            PortableSchema::GraphChange => {
                if envelope
                    .annotations()
                    .entries()
                    .any(|entry| entry.key() == "drasi.query.futures-due")
                {
                    return Err(BridgeError::invalid(
                        "query scheduling remains local to its instance",
                    ));
                }
                drasi_lib::computation::v1::GraphChangeCodec::decode_changes(envelope)
                    .map_err(|error| BridgeError::invalid(error.to_string()))?;
            }
            PortableSchema::QueryChange => {
                use drasi_lib::computation::v1::QueryChangeCodec;
                if QueryChangeCodec::is_snapshot(envelope)
                    || QueryChangeCodec::is_progress_only(envelope)
                {
                    return Err(BridgeError::invalid(
                        "snapshot/recovery control frames need an explicit protocol",
                    ));
                }
                let metadata = QueryChangeCodec::metadata(envelope)
                    .map_err(|error| BridgeError::invalid(error.to_string()))?;
                self.namespace.query(&metadata.query_id)?;
                QueryChangeCodec::decode_evaluation(envelope)
                    .map_err(|error| BridgeError::invalid(error.to_string()))?;
            }
            PortableSchema::SourceTransaction => {
                SourceTransactionCodec::decode(
                    envelope,
                    SourceTransactionLimits {
                        max_changes: std::num::NonZeroUsize::new(self.limits.max_operations)
                            .expect("validated operation limit"),
                        max_bytes: self.limits.envelope_limit(),
                        max_duration_ms: NonZeroU64::new(self.limits.timeout_ms)
                            .expect("validated deadline"),
                    },
                )
                .map_err(|error| BridgeError::invalid(error.to_string()))?;
            }
        }
        Ok(())
    }
}

fn identity(namespace: &str, bytes: &[u8]) -> Result<()> {
    if namespace.is_empty() || namespace.len() > 1024 || bytes.is_empty() || bytes.len() > 4096 {
        return Err(BridgeError::invalid("envelope identity limit"));
    }
    Ok(())
}

fn position(bytes: Option<&[u8]>) -> Result<()> {
    if bytes.is_some_and(|bytes| bytes.len() > 16 * 1024) {
        return Err(BridgeError::invalid("source position exceeds 16 KiB"));
    }
    Ok(())
}

fn annotation(key: &str, bytes: &[u8], limits: &Limits) -> Result<()> {
    if bytes.len() > 64 * 1024 {
        return Err(BridgeError::invalid("annotation bytes exceed 64 KiB"));
    }
    if key.starts_with("drasi.") {
        annotation_json(bytes, limits.max_depth)?;
    }
    Ok(())
}

/// Iterative framing check before serde can reserve from a hostile count.
/// Blob contents are never recursively visited as MessagePack.
pub(crate) fn preflight_messagepack(
    mut bytes: &[u8],
    limit: usize,
    max_values: usize,
) -> Result<()> {
    if bytes.len() > limit {
        return Err(BridgeError::invalid("MessagePack frame exceeds byte limit"));
    }
    fn take<'a>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
        if count > input.len() {
            return Err(BridgeError::invalid("truncated MessagePack frame"));
        }
        let (value, rest) = input.split_at(count);
        *input = rest;
        Ok(value)
    }
    fn length(input: &mut &[u8], width: usize) -> Result<usize> {
        Ok(take(input, width)?
            .iter()
            .fold(0usize, |value, byte| (value << 8) | usize::from(*byte)))
    }
    let mut remaining = vec![1usize];
    let mut visited = 0usize;
    while let Some(count) = remaining.last_mut() {
        if *count == 0 {
            remaining.pop();
            continue;
        }
        *count -= 1;
        visited += 1;
        if visited > max_values {
            return Err(BridgeError::invalid("MessagePack value budget exceeded"));
        }
        let marker = take(&mut bytes, 1)?[0];
        let mut children = 0;
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
            0xc4 | 0xd9 => length(&mut bytes, 1)?,
            0xc5 | 0xda => length(&mut bytes, 2)?,
            0xc6 | 0xdb => length(&mut bytes, 4)?,
            0xca | 0xce | 0xd2 => 4,
            0xcb | 0xcf | 0xd3 => 8,
            0xcc | 0xd0 => 1,
            0xcd | 0xd1 => 2,
            0xdc | 0xdd => {
                children = length(&mut bytes, if marker == 0xdc { 2 } else { 4 })?;
                0
            }
            0xde | 0xdf => {
                children = length(&mut bytes, if marker == 0xde { 2 } else { 4 })?
                    .checked_mul(2)
                    .ok_or_else(|| BridgeError::invalid("MessagePack count overflow"))?;
                0
            }
            _ => {
                return Err(BridgeError::invalid(
                    "reserved or extension MessagePack marker",
                ))
            }
        };
        take(&mut bytes, body)?;
        if children > bytes.len() || children > max_values.saturating_sub(visited) {
            return Err(BridgeError::invalid(
                "MessagePack count exceeds input or value budget",
            ));
        }
        if children > 0 {
            if remaining.len() >= 24 {
                return Err(BridgeError::invalid("MessagePack nesting exceeds 24"));
            }
            remaining.push(children);
        }
    }
    if !bytes.is_empty() {
        return Err(BridgeError::invalid("trailing MessagePack values"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(transparent)]
struct Blob<'a>(#[serde(borrow, with = "serde_bytes")] Cow<'a, [u8]>);

#[derive(Deserialize)]
struct Identity<'a> {
    #[serde(borrow)]
    namespace: Cow<'a, str>,
    #[serde(borrow)]
    value: Blob<'a>,
}

impl Identity<'_> {
    fn check(&self) -> Result<()> {
        identity(&self.namespace, &self.value.0)
    }
}

#[derive(Deserialize)]
struct Image<'a> {
    #[serde(borrow)]
    identity: Identity<'a>,
    #[serde(borrow)]
    bytes: Blob<'a>,
}

impl Image<'_> {
    fn check(&self, schema: PortableSchema, guard: &mut PayloadGuard<'_>) -> Result<()> {
        guard.identity(schema, &self.identity.namespace, &self.identity.value.0)?;
        guard.payload(schema, &self.bytes.0)
    }
}

#[derive(Deserialize)]
enum Operation<'a> {
    Add {
        #[serde(borrow)]
        after: Image<'a>,
    },
    Update {
        #[serde(borrow)]
        before: Option<Image<'a>>,
        #[serde(borrow)]
        after: Image<'a>,
    },
    Delete {
        #[serde(borrow)]
        identity: Identity<'a>,
        #[serde(borrow)]
        before: Option<Image<'a>>,
    },
}

#[derive(Deserialize)]
struct Metadata<'a> {
    #[serde(borrow)]
    stream: Cow<'a, str>,
    #[serde(borrow)]
    source_position: Option<Blob<'a>>,
}

impl Metadata<'_> {
    fn check(&self) -> Result<()> {
        crate::config::identifier(&self.stream)?;
        position(
            self.source_position
                .as_ref()
                .map(|position| position.0.as_ref()),
        )
    }
}

#[derive(Deserialize)]
enum Value<'a> {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    String(#[serde(borrow)] Cow<'a, str>),
    Bytes(#[serde(borrow)] Blob<'a>),
}

#[derive(Deserialize)]
struct Annotation<'a> {
    #[serde(borrow)]
    contributor: Cow<'a, str>,
    #[serde(borrow)]
    key: Cow<'a, str>,
    #[serde(borrow)]
    value: Value<'a>,
}

#[derive(Deserialize)]
struct Descriptor<'a> {
    #[serde(borrow)]
    id: Cow<'a, str>,
    version: u32,
    #[serde(borrow)]
    encoding: Cow<'a, str>,
    #[serde(borrow)]
    definition: Blob<'a>,
}

#[derive(Deserialize)]
struct EnvelopeProbe<'a> {
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
    #[serde(borrow)]
    annotations: Vec<Annotation<'a>>,
}

impl EnvelopeProbe<'_> {
    fn validate(&self, codec: &PortableEnvelopeCodec) -> Result<()> {
        let schema = codec.schema.schema();
        let expected = schema.descriptor();
        if self.format != BinaryEnvelopeCodec::VERSION
            || self.schema.id.as_ref() != expected.id().as_str()
            || self.schema.version != expected.version().value()
            || self.schema.encoding.as_ref() != expected.encoding()
            || self.schema.definition.0.as_ref() != expected.definition().as_ref()
            || self.operations.len() > codec.limits.max_operations
            || self.annotations.len() > codec.limits.max_annotations
            || self.lineage.len() > codec.limits.max_lineage
        {
            return Err(BridgeError::invalid(
                "wire schema, version or collection limit mismatch",
            ));
        }
        self.id.check()?;
        self.change_set.check()?;
        self.system.check()?;
        for (id, metadata) in &self.lineage {
            id.check()?;
            metadata.check()?;
        }
        for entry in &self.annotations {
            crate::config::identifier(&entry.contributor)?;
            crate::config::identifier(&entry.key)?;
            match &entry.value {
                Value::String(value) if value.len() > 64 * 1024 => {
                    return Err(BridgeError::invalid("annotation string limit"));
                }
                Value::Bytes(value) => annotation(&entry.key, &value.0, &codec.limits)?,
                Value::Bool(value) => {
                    let _ = value;
                }
                Value::Signed(value) => {
                    let _ = value;
                }
                Value::Unsigned(value) => {
                    let _ = value;
                }
                _ => {}
            }
        }
        let mut guard = PayloadGuard::new(&codec.limits, &codec.namespace);
        for operation in &self.operations {
            match operation {
                Operation::Add { after } => after.check(codec.schema, &mut guard)?,
                Operation::Update { before, after } => {
                    if let Some(before) = before {
                        before.check(codec.schema, &mut guard)?;
                    }
                    after.check(codec.schema, &mut guard)?;
                }
                Operation::Delete { identity, before } => {
                    guard.identity(codec.schema, &identity.namespace, &identity.value.0)?;
                    if let Some(before) = before {
                        before.check(codec.schema, &mut guard)?;
                    }
                }
            }
        }
        Ok(())
    }
}
