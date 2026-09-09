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
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::computation::v1::*;

pub fn schema_descriptor() -> SchemaDescriptor {
    named_schema("test.reading")
}

pub fn named_schema(name: &str) -> SchemaDescriptor {
    SchemaDescriptor::try_new(
        SchemaId::try_new(name).expect("schema ID"),
        SchemaVersion::try_new(1).expect("schema version"),
        "reading-u16be",
        Bytes::from_static(b"id:nonzero-u8,value:u16be;full-only"),
    )
    .expect("schema descriptor")
}

pub struct ReadingValidator(pub SchemaDescriptor);

impl RecordValidator for ReadingValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> std::result::Result<(), RecordValidationError> {
        if schema != &self.0
            || identity.namespace() != "readings"
            || identity.value().len() != 1
            || identity.value()[0] == 0
        {
            return Err(RecordValidationError::new(
                "identity",
                "expected matching schema and nonzero one-byte readings identity",
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
    ) -> std::result::Result<(), RecordValidationError> {
        self.validate_identity(schema, identity)?;
        if image != RecordImage::Full || payload.len() != 3 || payload[..1] != identity.value()[..]
        {
            return Err(RecordValidationError::new(
                "image",
                "expected full three-byte reading with matching embedded identity",
            ));
        }
        Ok(())
    }
}

pub fn changes(producer: &str, sequence: u64, values: &[u16]) -> ChangeSetRef {
    changes_with_schema(schema_descriptor(), producer, sequence, values)
}

pub fn changes_with_schema(
    descriptor: SchemaDescriptor,
    producer: &str,
    sequence: u64,
    values: &[u16],
) -> ChangeSetRef {
    assert!(
        !values.is_empty(),
        "use actual records, not empty placeholder batches"
    );
    let schema = Schema::new(descriptor.clone(), Arc::new(ReadingValidator(descriptor)));
    let operations = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let id = u8::try_from(index + 1).expect("record ID");
            let [high, low] = value.to_be_bytes();
            ChangeOperation::Added {
                ordinal: u64::from(id),
                after: Record::try_new(
                    &schema,
                    RecordId::try_new("readings", Bytes::copy_from_slice(&[id]))
                        .expect("record identity"),
                    RecordImage::Full,
                    Bytes::from(vec![id, high, low]),
                )
                .expect("validated record"),
            }
        })
        .collect();
    ChangeSet::try_new(
        ChangeSetId::try_new(producer, Bytes::copy_from_slice(&sequence.to_be_bytes()))
            .expect("batch identity"),
        schema.descriptor().clone(),
        operations,
    )
    .expect("validated batch")
}

pub fn values(envelope: &Envelope) -> Vec<u16> {
    envelope
        .changes()
        .operations()
        .iter()
        .map(|operation| {
            let ChangeOperation::Added { after, .. } = operation else {
                panic!("expected an added reading");
            };
            let [_, high, low] = after.payload().as_ref() else {
                panic!("invalid reading bytes");
            };
            u16::from_be_bytes([*high, *low])
        })
        .collect()
}

pub fn component(id: &str) -> ComponentId {
    ComponentId::try_new(id).expect("component ID")
}

pub fn port(id: &str) -> PortId {
    PortId::try_new(id).expect("port ID")
}

pub fn stream(producer: &str) -> StreamId {
    StreamId::try_new(format!("{producer}/out")).expect("stream ID")
}

pub fn endpoint(id: &str, name: &str) -> Endpoint {
    Endpoint::new(component(id), port(name))
}

pub fn edge(from: &str, to: &str) -> EdgeDefinition {
    EdgeDefinition::new(endpoint(from, "out"), endpoint(to, "in"))
}

pub fn descriptor(id: &str, inputs: &[&str], outputs: &[&str]) -> ComponentDescriptor {
    let mut ports = Vec::new();
    for (names, direction) in [(inputs, PortDirection::Input), (outputs, PortDirection::Output)] {
        for name in names {
            ports.push(PortDescriptor::new(
                port(name),
                direction,
                schema_descriptor(),
                PipeRequirements::default(),
            ));
        }
    }
    ComponentDescriptor::try_new(component(id), ports).expect("component descriptor")
}

pub fn annotation(id: &str) -> ContextEntry {
    ContextEntry::try_new(
        component(id),
        "visited",
        ContextValue::String(Arc::from(id)),
    )
    .expect("context contribution")
}

pub fn root(producer: &str, sequence: u64, readings: &[u16]) -> Envelope {
    // Deliberately opaque, non-UTF8 IDs, not the optional emission_id convention.
    let mut opaque_id = vec![0xff, 0, 0xfe];
    opaque_id.extend_from_slice(&sequence.to_le_bytes());
    let mut envelope = ChangeEnvelope::new(
        EnvelopeId::try_new(producer, Bytes::from(opaque_id)).expect("opaque envelope ID"),
        changes(producer, sequence, readings),
        SystemMetadata::new(stream(producer), sequence),
    );
    envelope
        .append_annotation(annotation(producer))
        .expect("source context");
    envelope
}

pub fn output(envelope: Envelope) -> OutputEnvelope {
    OutputEnvelope {
        port: port("out"),
        envelope,
    }
}

pub fn derived(input: &Envelope, producer: &str, sequence: u64, changes: ChangeSetRef) -> Envelope {
    let mut envelope = input.derive(
        emission_id(&stream(producer), sequence).expect("derived ID"),
        changes,
        SystemMetadata::new(stream(producer), sequence),
    );
    envelope
        .append_annotation(annotation(producer))
        .expect("branch context");
    envelope
}

pub struct FiniteSource {
    descriptor: ComponentDescriptor,
    outputs: VecDeque<OutputEnvelope>,
}

impl FiniteSource {
    pub fn new(id: &str, outputs: Vec<OutputEnvelope>) -> Self {
        Self {
            descriptor: descriptor(id, &[], &["out"]),
            outputs: outputs.into(),
        }
    }
}

type TransformFn =
    Box<dyn FnMut(InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> + Send + Sync>;

pub struct NativeTransform {
    descriptor: ComponentDescriptor,
    transform: TransformFn,
}

impl NativeTransform {
    pub fn new(
        id: &str,
        transform: impl FnMut(InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            descriptor: descriptor(id, &["in"], &["out"]),
            transform: Box::new(transform),
        }
    }
}

pub type Received = Arc<Mutex<Vec<InputEnvelope>>>;

pub struct CollectSink {
    descriptor: ComponentDescriptor,
    received: Received,
    annotate: bool,
}

impl CollectSink {
    pub fn new(id: &str, inputs: &[&str], received: Received) -> Self {
        Self {
            descriptor: descriptor(id, inputs, &[]),
            received,
            annotate: false,
        }
    }

    pub fn with_annotation(mut self) -> Self {
        self.annotate = true;
        self
    }
}

macro_rules! component_impl {
    ($type:ty) => {
        #[async_trait]
        impl ComputationComponent for $type {
            fn descriptor(&self) -> &ComponentDescriptor {
                &self.descriptor
            }

            async fn start(&mut self) -> anyhow::Result<()> {
                // Keep consumed input and emission counters across clean restarts.
                Ok(())
            }

            async fn stop(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
        }
    };
}

component_impl!(FiniteSource);
component_impl!(NativeTransform);
component_impl!(CollectSink);

#[async_trait]
impl EnvelopeSource for FiniteSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.outputs.pop_front())
    }
}

#[async_trait]
impl Transformer for NativeTransform {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        assert_eq!(input.port, port("in"));
        (self.transform)(input)
    }
}

#[async_trait]
impl EnvelopeSink for CollectSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, mut input: InputEnvelope) -> anyhow::Result<()> {
        if self.annotate {
            input
                .envelope
                .append_annotation(annotation(self.descriptor.id().as_str()))?;
        }
        self.received.lock().expect("sink lock").push(input);
        Ok(())
    }
}

// Observe send attempts, not sink calls: cancellation can otherwise conceal a
// wrongly forwarded prefix of an invalid transform batch. Transport is still
// the real bounded pipe, with its real receiver, capacity, and control handle.
pub struct AuditedBoundedPipe {
    pub sends: Arc<AtomicUsize>,
}

impl PipeProvider for AuditedBoundedPipe {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 1 }.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let provided = BoundedPipeConfig { capacity: 1 }.create()?;
        Ok(ProvidedPipe {
            pipe: Box::new(AuditedPipe {
                inner: provided.pipe,
                sends: self.sends.clone(),
            }),
            control: provided.control,
        })
    }
}

struct AuditedPipe {
    inner: Box<dyn Pipe>,
    sends: Arc<AtomicUsize>,
}

impl Pipe for AuditedPipe {
    fn capabilities(&self) -> &PipeCapabilities {
        self.inner.capabilities()
    }

    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        Arc::new(AuditedSender {
            inner: self.inner.sender(),
            sends: self.sends.clone(),
        })
    }

    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.inner.take_receiver()
    }
}

struct AuditedSender {
    inner: Arc<dyn EnvelopeSender>,
    sends: Arc<AtomicUsize>,
}

#[async_trait]
impl EnvelopeSender for AuditedSender {
    async fn send(&self, envelope: Envelope) -> std::result::Result<EnqueueReceipt, SendFailure> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.inner.send(envelope).await
    }
}
