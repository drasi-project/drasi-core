// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation")]

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
};
use drasi_lib::computation::v1::*;

struct Pass(Arc<AtomicUsize>);

impl SourceMiddlewareFactory for Pass {
    fn name(&self) -> String {
        "pass".into()
    }

    fn create(
        &self,
        _: &SourceMiddlewareConfig,
    ) -> std::result::Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(Self(self.0.clone())))
    }
}

#[async_trait]
impl SourceMiddleware for Pass {
    async fn process(
        &self,
        change: SourceChange,
        _: &dyn ElementIndex,
    ) -> std::result::Result<Vec<SourceChange>, MiddlewareError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![change])
    }
}

fn definition() -> MiddlewareTransformerDefinition {
    MiddlewareTransformerDefinition {
        id: ComponentId::try_new("normalize").expect("id"),
        output_stream: StreamId::try_new("normalize/out").expect("stream"),
        middleware: vec![SourceMiddlewareConfig::new("pass", "step", serde_json::Map::new())],
        pipeline: vec!["step".into()],
    }
}

fn endpoint(id: &str, port: &str) -> Endpoint {
    Endpoint::new(
        ComponentId::try_new(id).expect("id"),
        PortId::try_new(port).expect("port"),
    )
}

fn port_descriptor(id: &str, port: &str, direction: PortDirection) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        ComponentId::try_new(id).expect("id"),
        vec![PortDescriptor::new(
            PortId::try_new(port).expect("port"),
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("descriptor")
}

struct Source {
    descriptor: ComponentDescriptor,
    output: Option<OutputEnvelope>,
}

#[async_trait]
impl ComputationComponent for Source {
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
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.output.take())
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
}

#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.received.lock().expect("results").push(input.envelope);
        Ok(())
    }
}

#[tokio::test]
async fn declared_factory_uses_the_supplied_shared_registry_in_a_complete_graph() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(Pass(calls.clone())));
    let resource = ResourceId::try_new("middleware-registry").expect("resource");
    let definition = definition();
    let received = Arc::new(Mutex::new(Vec::new()));
    let input = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "one"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"value":42})),
            },
        },
        StreamId::try_new("source/out").expect("stream"),
        10,
        None,
    )
    .expect("input");
    let mut graph = ComputationGraph::builder("middleware-factory")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::Middleware,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from("middleware"),
        })
        .expect("declaration")
        .provide_resource(
            resource.clone(),
            ResourceHandle::new(
                ResourceRole::Middleware,
                Arc::new(QueryMiddlewareResource(Arc::new(registry))),
            ),
        )
        .expect("resource")
        .source(Box::new(Source {
            descriptor: port_descriptor("source", "out", PortDirection::Output),
            output: Some(OutputEnvelope {
                port: PortId::try_new("out").expect("port"),
                envelope: input,
            }),
        }))
        .component(
            definition.specification(resource).expect("specification"),
            Arc::new(MiddlewareTransformerFactory::default()),
        )
        .sink(Box::new(Sink {
            descriptor: port_descriptor("sink", "in", PortDirection::Input),
            received: received.clone(),
        }))
        .bind_stream(
            endpoint("source", "out"),
            StreamId::try_new("source/out").expect("stream"),
        )
        .bind_stream(endpoint("normalize", "out"), definition.output_stream)
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("normalize", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("normalize", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    graph.start().expect("start").await.expect("run");
    graph.dispose().await.expect("dispose");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let received = received.lock().expect("received");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].system().stream().as_str(), "normalize/out");
    assert_eq!(received[0].system().sequence(), 1);
    assert_eq!(
        GraphChangeCodec::decode_changes(&received[0])
            .expect("decode")
            .len(),
        1
    );
}

#[test]
fn factory_checks_definitions_and_registry_without_running_middleware() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(Pass(calls.clone())));
    let resource = ResourceId::try_new("registry").expect("resource");
    let resources = BTreeMap::from([(
        resource.clone(),
        ResourceHandle::new(
            ResourceRole::Middleware,
            Arc::new(MiddlewareRegistryResource(Arc::new(registry))),
        ),
    )]);
    let factory = MiddlewareTransformerFactory::default();
    let valid = definition().specification(resource).expect("specification");
    factory
        .validate_resources(&valid, &BTreeMap::new(), &resources)
        .expect("valid");
    for (key, value) in [
        ("stream", serde_json::json!("")),
        ("pipeline", serde_json::json!(["missing"])),
        ("pipeline", serde_json::json!(1)),
        (
            "middleware",
            serde_json::json!([{"kind":"unknown","name":"step","config":{}}]),
        ),
        (
            "middleware",
            serde_json::json!([
                {"kind":"pass","name":"step","config":{}},
                {"kind":"pass","name":"step","config":{}}
            ]),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid
            .configuration
            .insert(Arc::from(key), ConfigurationValue::Literal(value));
        assert!(
            factory
                .validate_resources(&invalid, &BTreeMap::new(), &resources)
                .is_err(),
            "{key} must fail"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn the_standard_factory_registry_already_contains_the_middleware_transformer() {
    assert!(FactoryRegistry::default()
        .register(Arc::new(MiddlewareTransformerFactory::default()))
        .is_ok());
    assert!(FactoryRegistry::standard()
        .register(Arc::new(MiddlewareTransformerFactory::default()))
        .is_err());
}

struct ExpandOrFilter;

impl SourceMiddlewareFactory for ExpandOrFilter {
    fn name(&self) -> String {
        "expand".into()
    }

    fn create(
        &self,
        _: &SourceMiddlewareConfig,
    ) -> std::result::Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(Self))
    }
}

#[async_trait]
impl SourceMiddleware for ExpandOrFilter {
    async fn process(
        &self,
        change: SourceChange,
        _: &dyn ElementIndex,
    ) -> std::result::Result<Vec<SourceChange>, MiddlewareError> {
        if let SourceChange::Insert {
            element:
                Element::Node {
                    metadata,
                    properties,
                },
        } = change
        {
            if matches!(
                properties["skip"],
                drasi_core::models::ElementValue::Bool(true)
            ) {
                return Ok(Vec::new());
            }
            Ok(["a", "b"]
                .into_iter()
                .map(|suffix| SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(
                                &metadata.reference.source_id,
                                &format!("{}-{suffix}", metadata.reference.element_id),
                            ),
                            ..metadata.clone()
                        },
                        properties: properties.clone(),
                    },
                })
                .collect())
        } else {
            Ok(vec![change])
        }
    }
}

fn raw_source_input(sequence: u64, skip: bool) -> InputEnvelope {
    let event = drasi_lib::channels::SourceEventWrapper::with_sequence(
        "source".into(),
        drasi_lib::channels::SourceEvent::Change(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::from(
                    serde_json::json!({"value":sequence, "skip":skip}),
                ),
            },
        }),
        chrono::DateTime::from_timestamp_millis(sequence as i64).expect("time"),
        sequence,
        None,
    );
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(event),
            &ComponentId::try_new("adapter").expect("id"),
            StreamId::try_new("source/out").expect("stream"),
            sequence + 1_000,
            None,
        )
        .expect("source event"),
    }
}

#[tokio::test]
async fn expansion_is_not_deduplicated_and_filtered_events_advance_downstream_progress() {
    use drasi_core::computation::InMemoryComputationProvider;
    use std::num::NonZeroUsize;

    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(ExpandOrFilter));
    let mut definition = definition();
    definition.middleware =
        vec![SourceMiddlewareConfig::new("expand", "step", serde_json::Map::new())];
    let mut transformer =
        MiddlewareTransformer::new(definition, Arc::new(registry)).expect("transformer");
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "middleware-progress".into(),
            id: ComponentId::try_new("query").expect("query"),
            query: "MATCH (n:Item) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").expect("stream"),
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .expect("query");
    transformer.start().await.expect("start transformer");
    query.start().await.expect("start query");

    let mut expanded = transformer
        .transform(raw_source_input(77, false))
        .await
        .expect("expand");
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0].envelope.changes().operations().len(), 2);
    let result = query
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: expanded.remove(0).envelope,
        })
        .await
        .expect("evaluate");
    assert_eq!(result.len(), 1);
    assert_eq!(
        QueryChangeCodec::to_legacy_result(&result[0].envelope)
            .expect("result")
            .results
            .len(),
        2
    );

    let mut filtered = transformer
        .transform(raw_source_input(78, true))
        .await
        .expect("filter");
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0].envelope.changes().is_empty());
    assert!(query
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: filtered.remove(0).envelope,
        })
        .await
        .expect("progress")
        .is_empty());
    assert!(query
        .transform(raw_source_input(78, false))
        .await
        .expect("duplicate filtered source position")
        .is_empty());
    transformer.stop().await.expect("stop transformer");
    query.stop().await.expect("stop query");
}
