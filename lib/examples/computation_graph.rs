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

//! Native finite computations: no Continuous Query or legacy source/reaction adapter.
//!
//! Run: `cargo run -p drasi-lib --features computation --example computation_graph`
//!
//! Expected output:
//! ```text
//! direct: input [2, 4, 6] -> [2, 4, 6] (Completed)
//! chain: input [2, 4, 6] -> double -> add one -> [5, 9, 13] (Completed)
//! ```
//! These bounded pipes are volatile; sink handling is not durable acknowledgement.

#![allow(clippy::print_stdout)]

use std::sync::{Arc, Mutex};

use anyhow::{ensure, Context};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::computation::v1::*;

// Application-defined encoding: [record ID: u8, value: big-endian u16].
struct ReadingValidator;

fn schema_descriptor() -> Result<SchemaDescriptor> {
    SchemaDescriptor::try_new(
        SchemaId::try_new("example.reading")?,
        SchemaVersion::try_new(1)?,
        "reading-u16be",
        Bytes::from_static(b"id:nonzero-u8,value:u16be;full-only"),
    )
}

impl RecordValidator for ReadingValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> std::result::Result<(), RecordValidationError> {
        let expected = schema_descriptor()
            .map_err(|error| RecordValidationError::new("schema", error.to_string()))?;
        if schema != &expected
            || identity.namespace() != "readings"
            || identity.value().len() != 1
            || identity.value()[0] == 0
        {
            return Err(RecordValidationError::new(
                "identity",
                "expected the reading schema and a nonzero one-byte readings identity",
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
                "expected a full three-byte reading with matching embedded identity",
            ));
        }
        Ok(())
    }
}

fn reading_changes(producer: &str, sequence: u64, id: u8, value: u16) -> Result<ChangeSetRef> {
    let schema = Schema::new(schema_descriptor()?, Arc::new(ReadingValidator));
    let [high, low] = value.to_be_bytes();
    let record = Record::try_new(
        &schema,
        RecordId::try_new("readings", Bytes::copy_from_slice(&[id]))?,
        RecordImage::Full,
        Bytes::from(vec![id, high, low]),
    )?;
    ChangeSet::try_new(
        ChangeSetId::try_new(producer, Bytes::copy_from_slice(&sequence.to_be_bytes()))?,
        schema.descriptor().clone(),
        vec![ChangeOperation::Added {
            ordinal: 0,
            after: record,
        }],
    )
}

fn readings(envelope: &Envelope) -> anyhow::Result<Vec<(u8, u16)>> {
    envelope
        .changes()
        .operations()
        .iter()
        .map(|operation| {
            let ChangeOperation::Added { after, .. } = operation else {
                anyhow::bail!("this example accepts append-only readings");
            };
            let [id, high, low] = after.payload().as_ref() else {
                anyhow::bail!("invalid validated reading");
            };
            Ok((*id, u16::from_be_bytes([*high, *low])))
        })
        .collect()
}

fn descriptor(id: &str, inputs: &[&str], outputs: &[&str]) -> Result<ComponentDescriptor> {
    let mut ports = Vec::new();
    for (names, direction) in [(inputs, PortDirection::Input), (outputs, PortDirection::Output)] {
        for name in names {
            ports.push(PortDescriptor::new(
                PortId::try_new(*name)?,
                direction,
                schema_descriptor()?,
                PipeRequirements::default(),
            ));
        }
    }
    ComponentDescriptor::try_new(ComponentId::try_new(id)?, ports)
}

fn endpoint(component: &str, port: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
}

struct FiniteReadings {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    values: Vec<u16>,
    cursor: usize,
    sequence: u64,
}

impl FiniteReadings {
    fn new(values: Vec<u16>) -> Result<Self> {
        Ok(Self {
            descriptor: descriptor("source", &[], &["out"])?,
            stream: StreamId::try_new("source/out")?,
            values,
            cursor: 0,
            sequence: 0,
        })
    }
}

#[async_trait]
impl ComputationComponent for FiniteReadings {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.cursor = 0;
        // A clean restart replays inputs, but must retain the stream's sequence.
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for FiniteReadings {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let Some(value) = self.values.get(self.cursor).copied() else {
            return Ok(None);
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("source sequence exhausted")?;
        let id = u8::try_from(self.cursor + 1).context("too many example readings")?;
        let envelope = Envelope::new(
            emission_id(&self.stream, self.sequence)?,
            reading_changes(self.stream.as_str(), self.sequence, id, value)?,
            SystemMetadata::new(self.stream.clone(), self.sequence),
        )
        .append_context(ContextEntry::try_new(
            self.descriptor.id().clone(),
            "origin",
            ContextValue::String(Arc::from("finite-readings")),
        )?)?;
        self.cursor += 1;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }))
    }
}

struct Arithmetic {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    multiply: u16,
    add: u16,
    sequence: u64,
}

impl Arithmetic {
    fn new(id: &str, multiply: u16, add: u16) -> Result<Self> {
        Ok(Self {
            descriptor: descriptor(id, &["in"], &["out"])?,
            stream: StreamId::try_new(format!("{id}/out"))?,
            multiply,
            add,
            sequence: 0,
        })
    }
}

#[async_trait]
impl ComputationComponent for Arithmetic {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for Arithmetic {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let annotated = input.envelope.append_context(ContextEntry::try_new(
            self.descriptor.id().clone(),
            "stage",
            ContextValue::String(Arc::from(self.descriptor.id().as_str())),
        )?)?;
        let mut outputs = Vec::new();
        for (id, value) in readings(&input.envelope)? {
            let value = value
                .checked_mul(self.multiply)
                .and_then(|value| value.checked_add(self.add))
                .context("reading arithmetic overflow")?;
            self.sequence = self
                .sequence
                .checked_add(1)
                .context("transform sequence exhausted")?;
            outputs.push(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: annotated.derive(
                    emission_id(&self.stream, self.sequence)?,
                    reading_changes(self.stream.as_str(), self.sequence, id, value)?,
                    SystemMetadata::new(self.stream.clone(), self.sequence),
                ),
            });
        }
        Ok(outputs)
    }
}

struct CollectReadings {
    descriptor: ComponentDescriptor,
    values: Arc<Mutex<Vec<u16>>>,
}

#[async_trait]
impl ComputationComponent for CollectReadings {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for CollectReadings {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let values = readings(&input.envelope)?;
        let mut collected = self
            .values
            .lock()
            .map_err(|_| anyhow::anyhow!("sink lock poisoned"))?;
        collected.extend(values.into_iter().map(|(_, value)| value));
        Ok(())
    }
}

async fn run_pipeline(with_transforms: bool) -> anyhow::Result<()> {
    let inputs = vec![2, 4, 6];
    let values = Arc::new(Mutex::new(Vec::new()));
    let mut builder = ComputationGraph::builder(if with_transforms { "chain" } else { "direct" })
        .source(Box::new(FiniteReadings::new(inputs.clone())?))
        .sink(Box::new(CollectReadings {
            descriptor: descriptor("sink", &["in"], &[])?,
            values: values.clone(),
        }))
        .bind_stream(endpoint("source", "out")?, StreamId::try_new("source/out")?);
    let edges = if with_transforms {
        builder = builder
            .transformer(Box::new(Arithmetic::new("double", 2, 0)?))
            .transformer(Box::new(Arithmetic::new("add-one", 1, 1)?))
            .bind_stream(endpoint("double", "out")?, StreamId::try_new("double/out")?)
            .bind_stream(
                endpoint("add-one", "out")?,
                StreamId::try_new("add-one/out")?,
            );
        vec![("source", "double"), ("double", "add-one"), ("add-one", "sink")]
    } else {
        vec![("source", "sink")]
    };
    for (from, to) in edges {
        builder = builder.connect(
            EdgeDefinition::new(endpoint(from, "out")?, endpoint(to, "in")?),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    }
    let mut graph = builder.build()?;
    let run = graph.start()?;
    let control = run.control();
    run.await?;
    ensure!(control.state() == GraphState::Completed);
    ensure!(graph.state() == GraphState::Completed);
    let results = values
        .lock()
        .map_err(|_| anyhow::anyhow!("sink lock poisoned"))?;
    if with_transforms {
        ensure!(*results == [5, 9, 13]);
        println!("chain: input {inputs:?} -> double -> add one -> {results:?} (Completed)");
    } else {
        ensure!(*results == inputs);
        println!("direct: input {inputs:?} -> {results:?} (Completed)");
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    run_pipeline(false).await?;
    run_pipeline(true).await
}
