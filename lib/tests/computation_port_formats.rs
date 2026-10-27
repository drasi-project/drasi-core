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

//! Per-port format overrides through the ordinary factory/specification path.
//! Both formats are real, validated schemas the test factories implement; the
//! graph connects matching custom descriptors directly and never converts.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use computation_support::*;
use drasi_lib::computation::v1::*;
use serde_json::json;

fn default_schema() -> SchemaDescriptor {
    named_schema("test.reading")
}

fn custom_schema() -> SchemaDescriptor {
    named_schema("test.alt-reading")
}

const CUSTOM_FORMAT: &str = "test.alt-reading-v1";

struct FormatSource {
    descriptor: ComponentDescriptor,
    output: Option<OutputEnvelope>,
}

struct FormatSink {
    descriptor: ComponentDescriptor,
    received: Received,
}

macro_rules! component_impl {
    ($type:ty) => {
        #[async_trait]
        impl ComputationComponent for $type {
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
    };
}

component_impl!(FormatSource);
component_impl!(FormatSink);

#[async_trait]
impl EnvelopeSource for FormatSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.output.take())
    }
}

#[async_trait]
impl EnvelopeSink for FormatSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.received.lock().expect("sink lock").push(input);
        Ok(())
    }
}

fn envelope(schema: SchemaDescriptor, producer: &str, readings: &[u16]) -> Envelope {
    let mut envelope = ChangeEnvelope::new(
        EnvelopeId::try_new(producer, Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0, 1]))
            .expect("envelope ID"),
        changes_with_schema(schema, producer, 1, readings),
        SystemMetadata::new(stream(producer), 1),
    );
    envelope
        .append_annotation(annotation(producer))
        .expect("source context");
    envelope
}

/// A source or sink factory with one port that supports the default reading
/// format and the custom one. `creates` counts every construction.
struct FormatFactory {
    descriptor: FactoryDescriptor,
    catalogue: PortFormatCatalogue,
    primaries: PortPrimaries,
    creates: Arc<AtomicUsize>,
    received: Received,
}

impl FormatFactory {
    fn new(role: ComponentRole, creates: Arc<AtomicUsize>, received: Received) -> Arc<Self> {
        Self::designating(role, creates, received, None)
    }

    /// `primaries` defaults to designating the factory's only port.
    fn designating(
        role: ComponentRole,
        creates: Arc<AtomicUsize>,
        received: Received,
        primaries: Option<PortPrimaries>,
    ) -> Arc<Self> {
        let (port_name, direction) = match role {
            ComponentRole::Source => ("out", PortDirection::Output),
            ComponentRole::Sink => ("in", PortDirection::Input),
            _ => unreachable!("test factory roles"),
        };
        let primaries = primaries.unwrap_or_else(|| match direction {
            PortDirection::Input => PortPrimaries::new(Some(port(port_name)), None),
            PortDirection::Output => PortPrimaries::new(None, Some(port(port_name))),
        });
        let catalogue = PortFormatCatalogue::try_new(
            [PortDescriptor::new(
                port(port_name),
                direction,
                default_schema(),
                PipeRequirements::default(),
            )],
            [PortFormatAlternative {
                port: port(port_name),
                schema: custom_schema(),
            }],
        )
        .expect("port format catalogue");
        Arc::new(Self {
            descriptor: FactoryDescriptor {
                implementation: Self::implementation(role),
                role,
                configuration_version: 1,
                configuration: ConfigurationSchema::default().with_port_formats(),
                dependencies: BTreeMap::new(),
            },
            catalogue,
            primaries,
            creates,
            received,
        })
    }

    fn implementation(role: ComponentRole) -> ImplementationIdentity {
        ImplementationIdentity::try_new(format!("test/format-{role:?}"), "1").expect("identity")
    }
}

#[async_trait]
impl ComponentFactory for FormatFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn port_formats(&self) -> Option<&PortFormatCatalogue> {
        Some(&self.catalogue)
    }

    fn primary_ports(&self) -> PortPrimaries {
        self.primaries.clone()
    }

    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        let configuration: BTreeMap<Arc<str>, serde_json::Value> =
            serde_json::from_value(configuration)?;
        let role = self.descriptor.role;
        Ok(ComponentSpecification {
            descriptor: self
                .catalogue
                .describe(id, configuration.get(PORT_FORMATS_KEY))?,
            role,
            completion: (role == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: configuration
                .into_iter()
                .map(|(name, value)| (name, ConfigurationValue::Literal(value)))
                .collect(),
            dependencies: BTreeMap::new(),
        })
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        let descriptor = context.specification.descriptor.clone();
        let schema = descriptor.ports()[0].schema().clone();
        Ok(match self.descriptor.role {
            ComponentRole::Source => ConstructedComponent::source(Box::new(FormatSource {
                output: Some(OutputEnvelope {
                    port: port("out"),
                    envelope: envelope(schema, context.component_id.as_str(), &[7]),
                }),
                descriptor,
            })),
            _ => ConstructedComponent::sink(Box::new(FormatSink {
                descriptor,
                received: self.received.clone(),
            })),
        })
    }
}

struct Fixture {
    creates: Arc<AtomicUsize>,
    received: Received,
    source: Arc<FormatFactory>,
    sink: Arc<FormatFactory>,
}

impl Fixture {
    fn new() -> Self {
        let creates = Arc::new(AtomicUsize::new(0));
        let received = Received::default();
        Self {
            source: FormatFactory::new(ComponentRole::Source, creates.clone(), received.clone()),
            sink: FormatFactory::new(ComponentRole::Sink, creates.clone(), received.clone()),
            creates,
            received,
        }
    }

    fn specifications(
        &self,
        source: serde_json::Value,
        sink: serde_json::Value,
    ) -> anyhow::Result<(ComponentSpecification, ComponentSpecification)> {
        Ok((
            self.source
                .specification_from_config(component("source"), source)?,
            self.sink
                .specification_from_config(component("sink"), sink)?,
        ))
    }

    fn build(
        &self,
        source: ComponentSpecification,
        sink: ComponentSpecification,
    ) -> GraphResult<ComputationGraph> {
        ComputationGraph::builder("port-formats")
            .component(source, self.source.clone())
            .component(sink, self.sink.clone())
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "sink"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
    }

    async fn run(&self, mut graph: ComputationGraph) {
        tokio::time::timeout(Duration::from_secs(5), graph.start().expect("start"))
            .await
            .expect("graph did not complete")
            .expect("graph run");
        assert_eq!(graph.state(), GraphState::Completed);
        graph.dispose().await.expect("dispose");
    }

    fn delivered_schemas(&self) -> Vec<SchemaDescriptor> {
        self.received
            .lock()
            .expect("received")
            .iter()
            .map(|input| input.envelope.changes().schema().clone())
            .collect()
    }
}

#[tokio::test]
async fn default_ports_are_unchanged_without_overrides() {
    let fixture = Fixture::new();
    let (source, sink) = fixture.specifications(json!({}), json!({})).expect("specs");
    assert_eq!(source.descriptor.ports()[0].schema(), &default_schema());
    assert_eq!(sink.descriptor.ports()[0].schema(), &default_schema());
    fixture
        .run(fixture.build(source, sink).expect("default graph"))
        .await;
    assert_eq!(fixture.delivered_schemas(), [default_schema()]);
    assert_eq!(fixture.creates.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn explicit_default_selection_matches_the_default() {
    let fixture = Fixture::new();
    let (source, sink) = fixture
        .specifications(
            json!({"ports": {"out": {"format": "test.reading-v1"}}}),
            json!({}),
        )
        .expect("specs");
    assert_eq!(source.descriptor.ports()[0].schema(), &default_schema());
    fixture
        .run(fixture.build(source, sink).expect("graph"))
        .await;
    assert_eq!(fixture.delivered_schemas(), [default_schema()]);
}

#[tokio::test]
async fn supported_custom_format_pair_connects_directly() {
    let fixture = Fixture::new();
    let (source, sink) = fixture
        .specifications(
            json!({"ports": {"out": {"format": CUSTOM_FORMAT}}}),
            json!({"ports": {"in": {"format": CUSTOM_FORMAT}}}),
        )
        .expect("specs");
    assert_eq!(source.descriptor.ports()[0].schema(), &custom_schema());
    assert_eq!(sink.descriptor.ports()[0].schema(), &custom_schema());
    // The primary designation names the port, so an override does not move it.
    assert_eq!(
        fixture
            .source
            .primary_ports()
            .resolve(source.descriptor.ports(), PortDirection::Output, None)
            .expect("primary output"),
        port("out")
    );
    assert_eq!(
        fixture
            .sink
            .primary_ports()
            .resolve(sink.descriptor.ports(), PortDirection::Input, None)
            .expect("primary input"),
        port("in")
    );
    fixture
        .run(fixture.build(source, sink).expect("custom graph"))
        .await;
    assert_eq!(fixture.delivered_schemas(), [custom_schema()]);
    let received = fixture.received.lock().expect("received");
    assert_eq!(values(&received[0].envelope), [7]);
}

#[test]
fn invalid_primary_designations_are_rejected_before_any_construction() {
    for (fault, primaries) in [
        ("missing", PortPrimaries::new(None, Some(port("missing")))),
        (
            "wrong direction",
            PortPrimaries::new(Some(port("out")), None),
        ),
    ] {
        let mut fixture = Fixture::new();
        fixture.source = FormatFactory::designating(
            ComponentRole::Source,
            fixture.creates.clone(),
            fixture.received.clone(),
            Some(primaries),
        );
        let (source, sink) = fixture.specifications(json!({}), json!({})).expect("specs");
        assert!(fixture.build(source, sink).is_err(), "{fault}");
        assert_eq!(fixture.creates.load(Ordering::SeqCst), 0, "{fault}");
    }
}

#[test]
fn mismatched_formats_are_not_converted() {
    let fixture = Fixture::new();
    let (source, sink) = fixture
        .specifications(
            json!({"ports": {"out": {"format": CUSTOM_FORMAT}}}),
            json!({}),
        )
        .expect("specs");
    assert!(fixture.build(source, sink).is_err());
    assert_eq!(fixture.creates.load(Ordering::SeqCst), 0);
}

#[test]
fn unsupported_overrides_fail_when_describing_the_configuration() {
    let fixture = Fixture::new();
    for (configuration, expected) in [
        (
            json!({"ports": {"out": {"format": "test.unknown-v1"}}}),
            "supported formats",
        ),
        (
            json!({"ports": {"out": {"format": "test.reading-v2"}}}),
            "test.reading-v1",
        ),
        (
            json!({"ports": {"missing": {"format": "test.reading-v1"}}}),
            "not declared",
        ),
        (
            json!({"ports": {"out": {}}}),
            "invalid port format override",
        ),
    ] {
        let error = fixture
            .source
            .specification_from_config(component("source"), configuration.clone())
            .expect_err("unsupported override");
        assert!(
            error.to_string().contains(expected),
            "{configuration}: {error}"
        );
    }
    // The sink's input port does not support another component's output name.
    assert!(fixture
        .sink
        .specification_from_config(
            component("sink"),
            json!({"ports": {"out": {"format": "test.reading-v1"}}}),
        )
        .is_err());
    assert_eq!(fixture.creates.load(Ordering::SeqCst), 0);
}

#[test]
fn handwritten_specifications_are_checked_before_any_construction() {
    for fault in [
        "unsupported-format",
        "unknown-port",
        "override-without-descriptor",
        "descriptor-without-override",
        "non-object",
    ] {
        let fixture = Fixture::new();
        let (mut source, mut sink) = fixture.specifications(json!({}), json!({})).expect("specs");
        let literal = ConfigurationValue::Literal;
        match fault {
            "unsupported-format" => {
                source.configuration.insert(
                    Arc::from("ports"),
                    literal(json!({"out": {"format": "test.unknown-v1"}})),
                );
            }
            "unknown-port" => {
                source.configuration.insert(
                    Arc::from("ports"),
                    literal(json!({"missing": {"format": CUSTOM_FORMAT}})),
                );
            }
            "override-without-descriptor" => {
                source.configuration.insert(
                    Arc::from("ports"),
                    literal(json!({"out": {"format": CUSTOM_FORMAT}})),
                );
            }
            "descriptor-without-override" => {
                source.descriptor = ComponentDescriptor::try_new(
                    component("source"),
                    vec![PortDescriptor::new(
                        port("out"),
                        PortDirection::Output,
                        custom_schema(),
                        PipeRequirements::default(),
                    )],
                )
                .expect("descriptor");
                sink = fixture
                    .sink
                    .specification_from_config(
                        component("sink"),
                        json!({"ports": {"in": {"format": CUSTOM_FORMAT}}}),
                    )
                    .expect("sink");
            }
            "non-object" => {
                source
                    .configuration
                    .insert(Arc::from("ports"), literal(json!("out")));
            }
            _ => unreachable!(),
        }
        // Keep the sink compatible so only the source specification is at fault.
        let result = fixture.build(source, sink);
        assert!(result.is_err(), "{fault}");
        assert_eq!(fixture.creates.load(Ordering::SeqCst), 0, "{fault}");
    }
}
