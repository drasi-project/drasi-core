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

#![cfg(feature = "computation")]

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    interface::{
        ElementIndex, FutureElementRef, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
        SourceMiddlewareConfig,
    },
};
use drasi_lib::{
    channels::{SourceEvent, SourceEventWrapper},
    computation::v1::*,
    profiling::ProfilingMetadata,
    schema::SourceSchema,
};
use serde::Deserialize;
use serde_json::{json, Value};

fn component(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("component ID")
}

fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port ID")
}

fn stream(producer: &str) -> StreamId {
    StreamId::try_new(format!("{producer}/out")).expect("stream ID")
}

fn endpoint(producer: &str, name: &str) -> Endpoint {
    Endpoint::new(component(producer), port(name))
}

fn config(kind: &str, name: &str, value: Value) -> SourceMiddlewareConfig {
    SourceMiddlewareConfig::new(
        kind,
        name,
        value.as_object().expect("config object").clone(),
    )
}

fn definition(
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: &[&str],
) -> MiddlewareTransformerDefinition {
    MiddlewareTransformerDefinition {
        id: component("middleware"),
        output_stream: stream("middleware"),
        middleware,
        pipeline: pipeline.iter().map(|name| (*name).to_owned()).collect(),
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum FixtureOperation {
    Add { operand: i64 },
    Multiply { operand: i64 },
    Filter,
}

struct FixtureMiddleware(FixtureOperation);

#[async_trait]
impl SourceMiddleware for FixtureMiddleware {
    async fn process(
        &self,
        mut change: SourceChange,
        _index: &dyn ElementIndex,
    ) -> std::result::Result<Vec<SourceChange>, MiddlewareError> {
        if matches!(self.0, FixtureOperation::Filter) {
            return Ok(Vec::new());
        }
        if let SourceChange::Insert { element } | SourceChange::Update { element } = &mut change {
            let properties = match element {
                Element::Node { properties, .. } | Element::Relation { properties, .. } => {
                    properties
                }
            };
            let Some(ElementValue::Integer(value)) = properties.get("value") else {
                return Err(MiddlewareError::SourceChangeError(
                    "fixture requires an integer value".into(),
                ));
            };
            let value = match self.0 {
                FixtureOperation::Add { operand } => value + operand,
                FixtureOperation::Multiply { operand } => value * operand,
                FixtureOperation::Filter => unreachable!("handled above"),
            };
            properties.insert("value", ElementValue::Integer(value));
        }
        Ok(vec![change])
    }
}

struct FixtureFactory;

impl SourceMiddlewareFactory for FixtureFactory {
    fn name(&self) -> String {
        "fixture".into()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> std::result::Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let operation =
            serde_json::from_value(Value::Object(config.config.clone())).map_err(|error| {
                MiddlewareSetupError::InvalidConfiguration(format!("fixture config: {error}"))
            })?;
        Ok(Arc::new(FixtureMiddleware(operation)))
    }
}

fn registry() -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(FixtureFactory));
    #[cfg(any(feature = "middleware-decoder", feature = "middleware-all"))]
    registry.register(Arc::new(drasi_middleware::decoder::DecoderFactory::new()));
    #[cfg(any(feature = "middleware-parse-json", feature = "middleware-all"))]
    registry.register(Arc::new(
        drasi_middleware::parse_json::ParseJsonFactory::new(),
    ));
    #[cfg(any(feature = "middleware-promote", feature = "middleware-all"))]
    registry.register(Arc::new(
        drasi_middleware::promote::PromoteMiddlewareFactory::new(),
    ));
    #[cfg(any(feature = "middleware-relabel", feature = "middleware-all"))]
    registry.register(Arc::new(
        drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
    ));
    #[cfg(any(feature = "middleware-map", feature = "middleware-all"))]
    registry.register(Arc::new(drasi_middleware::map::MapFactory::new()));
    #[cfg(any(
        feature = "middleware-jq",
        feature = "middleware-bundled-jq",
        feature = "middleware-all"
    ))]
    registry.register(Arc::new(drasi_middleware::jq::JQFactory::new()));
    #[cfg(any(feature = "middleware-unwind", feature = "middleware-all"))]
    registry.register(Arc::new(drasi_middleware::unwind::UnwindFactory::new()));
    Arc::new(registry)
}

async fn running(
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: &[&str],
) -> MiddlewareTransformer {
    let mut subject = MiddlewareTransformer::new(definition(middleware, pipeline), registry())
        .expect("construct middleware transformer");
    subject.start().await.expect("start middleware transformer");
    subject
}

fn metadata(source: &str, id: &str, label: &str, time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(source, id),
        labels: Arc::from([Arc::from(label)]),
        effective_from: time,
    }
}

fn node(source: &str, id: &str, label: &str, time: u64, properties: Value) -> Element {
    Element::Node {
        metadata: metadata(source, id, label, time),
        properties: ElementPropertyMap::from(properties),
    }
}

fn mutation(element: Element, update: bool) -> SourceChange {
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn event(producer: &str, sequence: u64, change: SourceChange) -> ChangeEnvelope {
    let source = Arc::new(SourceEventWrapper {
        source_id: format!("raw-{producer}"),
        event: SourceEvent::Change(change),
        timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 123_456_789).expect("timestamp"),
        sequence: Some(sequence + 1000),
        source_position: Some(Bytes::from_static(&[0xff, 0, 7])),
        profiling: Some(ProfilingMetadata {
            source_ns: Some(11),
            source_receive_ns: Some(12),
            source_send_ns: Some(13),
            ..Default::default()
        }),
    });
    let mut envelope = GraphChangeCodec::encode_source_event(
        source,
        &component(producer),
        stream(producer),
        sequence,
        Some(SourceSchema::default()),
    )
    .expect("encode source event");
    envelope
        .append_annotation(
            ContextEntry::try_new(
                component(producer),
                "branch",
                ContextValue::String(Arc::from(producer)),
            )
            .expect("branch annotation"),
        )
        .expect("append annotation");
    envelope
}

fn record(change: SourceChange) -> Record {
    let envelope = GraphChangeCodec::encode_change(change, stream("record"), 1, None)
        .expect("encode graph record");
    match &envelope.changes().operations()[0] {
        ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
            after.clone()
        }
        ChangeOperation::Deleted {
            before: Some(before),
            ..
        } => before.clone(),
        _ => panic!("source change must have a typed record"),
    }
}

fn derived_input(
    input: &ChangeEnvelope,
    producer: &str,
    sequence: u64,
    operations: Vec<ChangeOperation>,
) -> ChangeEnvelope {
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new(producer, Bytes::copy_from_slice(&sequence.to_be_bytes()))
            .expect("change-set ID"),
        GraphChangeCodec::schema().descriptor().clone(),
        operations,
    )
    .expect("graph batch");
    let mut system = SystemMetadata::new(stream(producer), sequence);
    if let Some(timestamp) = input.system().timestamp() {
        system = system.with_timestamp(timestamp);
    }
    if let Some(position) = input.system().source_position() {
        system = system.with_source_position(position.clone());
    }
    input.derive(
        emission_id(&stream(producer), sequence).expect("derived input ID"),
        changes,
        system,
    )
}

fn assert_derived(output: &ChangeEnvelope, input: &ChangeEnvelope) {
    assert_ne!(output.id(), input.id());
    assert!(!Arc::ptr_eq(output.event(), input.event()));
    assert_eq!(output.system().stream(), &stream("middleware"));
    assert_eq!(output.system().timestamp(), input.system().timestamp());
    assert_eq!(
        output.system().source_position(),
        input.system().source_position()
    );
    assert_eq!(
        GraphChangeCodec::source_metadata(output).expect("output metadata"),
        GraphChangeCodec::source_metadata(input).expect("input metadata")
    );
    let original_annotations: Vec<_> = input.annotations().entries().collect();
    let output_annotations: Vec<_> = output.annotations().entries().collect();
    assert!(output_annotations.ends_with(&original_annotations));
    let lineage = output.lineage().expect("input lineage");
    assert_eq!(lineage.envelope_id(), input.id());
    assert_eq!(lineage.system(), input.system());
    let mut expected = input.lineage();
    let mut actual = lineage.parent();
    while let (Some(expected_entry), Some(actual_entry)) = (expected, actual) {
        assert_eq!(actual_entry.envelope_id(), expected_entry.envelope_id());
        assert_eq!(actual_entry.system(), expected_entry.system());
        expected = expected_entry.parent();
        actual = actual_entry.parent();
    }
    assert!(expected.is_none() && actual.is_none(), "complete ancestry");
}

async fn transform_one(
    subject: &mut MiddlewareTransformer,
    input: &ChangeEnvelope,
) -> ChangeEnvelope {
    let original_operations = input.changes().operations().to_vec();
    let original_annotations: Vec<_> = input.annotations().entries().collect();
    let mut outputs = subject
        .transform(InputEnvelope {
            port: port("in"),
            envelope: input.clone(),
        })
        .await
        .expect("transform graph event");
    assert_eq!(
        outputs.len(),
        1,
        "one output batch per input, even if empty"
    );
    let output = outputs.remove(0);
    assert_eq!(output.port, port("out"));
    assert_eq!(
        output.envelope.changes().schema(),
        GraphChangeCodec::schema().descriptor()
    );
    assert_derived(&output.envelope, input);
    assert_eq!(input.changes().operations(), original_operations);
    assert_eq!(
        input.annotations().entries().collect::<Vec<_>>(),
        original_annotations
    );
    output.envelope
}

fn decoded(envelope: &ChangeEnvelope) -> Vec<SourceChange> {
    GraphChangeCodec::decode_changes(envelope).expect("decode transformed graph changes")
}

async fn assert_future_passthrough(subject: &mut MiddlewareTransformer) {
    let future = SourceChange::Future {
        future_ref: FutureElementRef {
            element_ref: ElementReference::new("database", "future-item"),
            original_time: 100,
            due_time: 200,
            group_signature: 77,
        },
    };
    let output = transform_one(subject, &event("producer", 900, future.clone())).await;
    assert_eq!(decoded(&output), [future]);
    assert!(matches!(
        output.changes().operations(),
        [ChangeOperation::Updated {
            semantics: UpdateSemantics::Patch,
            ..
        }]
    ));
}

async fn assert_element_mutations(
    middleware: SourceMiddlewareConfig,
    original_properties: Value,
    expected_properties: Value,
    original_label: &str,
    expected_label: &str,
) {
    let name = middleware.name.to_string();
    let mut subject = running(vec![middleware], &[&name]).await;
    let mut previous_sequence = None;
    for (index, update) in [false, true].into_iter().enumerate() {
        let time = 100 + index as u64;
        let input = event(
            "producer",
            40 + index as u64,
            mutation(
                node(
                    "database",
                    "item",
                    original_label,
                    time,
                    original_properties.clone(),
                ),
                update,
            ),
        );
        let output = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&output),
            [mutation(
                node(
                    "database",
                    "item",
                    expected_label,
                    time,
                    expected_properties.clone(),
                ),
                update,
            )]
        );
        if update {
            assert!(matches!(
                output.changes().operations(),
                [ChangeOperation::Updated {
                    semantics: UpdateSemantics::Patch,
                    ..
                }]
            ));
        }
        if let Some(previous) = previous_sequence {
            assert!(output.system().sequence() > previous);
        }
        previous_sequence = Some(output.system().sequence());
    }
    let delete = event(
        "producer",
        42,
        SourceChange::Delete {
            metadata: metadata("database", "item", original_label, 102),
        },
    );
    let output = transform_one(&mut subject, &delete).await;
    assert_eq!(
        decoded(&output),
        [SourceChange::Delete {
            metadata: metadata("database", "item", expected_label, 102),
        }]
    );
    assert!(output.system().sequence() > previous_sequence.expect("prior output"));
    assert_future_passthrough(&mut subject).await;
    subject.stop().await.expect("stop");
}

async fn assert_processing_failure(
    middleware: SourceMiddlewareConfig,
    invalid: &ChangeEnvelope,
    valid: &ChangeEnvelope,
    message: &str,
) -> ChangeEnvelope {
    let name = middleware.name.to_string();
    let mut subject = running(vec![middleware.clone()], &[&name]).await;
    let original = invalid.changes().operations().to_vec();
    let error = subject
        .transform(InputEnvelope {
            port: port("in"),
            envelope: invalid.clone(),
        })
        .await
        .expect_err("processing must fail, not emit an empty or partially transformed batch");
    assert!(
        format!("{error:#}").contains(message),
        "underlying middleware error must remain visible: {error:#}"
    );
    assert_eq!(invalid.changes().operations(), original);
    assert!(
        subject
            .transform(InputEnvelope {
                port: port("in"),
                envelope: valid.clone(),
            })
            .await
            .is_err(),
        "a failed instance cannot resume processing"
    );
    subject.stop().await.expect("stop failed instance");
    assert!(
        subject.start().await.is_err(),
        "stop/start must not clear the failed-processing fence"
    );
    let mut replacement = running(vec![middleware], &[&name]).await;
    let output = transform_one(&mut replacement, valid).await;
    replacement.stop().await.expect("stop replacement");
    output
}

#[test]
fn descriptor_has_only_graph_change_input_and_output_ports() {
    let definition = definition(Vec::new(), &[]);
    let descriptor = definition.descriptor();
    assert_eq!(descriptor.id(), &component("middleware"));
    assert_eq!(descriptor.ports().len(), 2);
    for (name, direction) in [("in", PortDirection::Input), ("out", PortDirection::Output)] {
        let declared = descriptor
            .ports()
            .iter()
            .find(|candidate| candidate.id() == &port(name))
            .expect("declared port");
        assert_eq!(declared.direction(), direction);
        assert_eq!(declared.schema(), GraphChangeCodec::schema().descriptor());
    }
    let subject = MiddlewareTransformer::new(definition, registry()).expect("construct");
    assert_eq!(subject.descriptor(), &descriptor);
}

fn assert_setup_error(middleware: Vec<SourceMiddlewareConfig>, pipeline: &[&str], message: &str) {
    let error = MiddlewareTransformer::new(definition(middleware, pipeline), registry())
        .err()
        .expect("invalid middleware definition must fail during construction");
    assert!(
        format!("{error:#}").to_lowercase().contains(message),
        "useful setup error expected: {error:#}"
    );
}

#[test]
fn setup_rejects_unknown_kinds_unknown_names_and_duplicate_definitions() {
    assert_setup_error(
        vec![config("not_registered", "unknown", json!({}))],
        &["unknown"],
        "not_registered",
    );
    assert_setup_error(Vec::new(), &["missing_instance"], "missing_instance");
    let middleware = config("fixture", "same_name", json!({"operation": "filter"}));
    assert_setup_error(
        vec![middleware.clone(), middleware],
        &["same_name"],
        "duplicate",
    );
}

#[test]
fn setup_rejects_invalid_configuration_for_every_enabled_factory() {
    let mut cases = vec![("fixture", json!({"operation": "unknown"}))];
    #[cfg(any(feature = "middleware-decoder", feature = "middleware-all"))]
    cases.push(("decoder", json!({"encoding_type": "not-an-encoding"})));
    #[cfg(any(feature = "middleware-parse-json", feature = "middleware-all"))]
    cases.push(("parse_json", json!({"target_property": ""})));
    #[cfg(any(feature = "middleware-promote", feature = "middleware-all"))]
    cases.push(("promote", json!({"mappings": []})));
    #[cfg(any(feature = "middleware-relabel", feature = "middleware-all"))]
    cases.push(("relabel", json!({"labelMappings": {}})));
    #[cfg(any(feature = "middleware-map", feature = "middleware-all"))]
    cases.push(("map", json!({"Raw": {"insert": true}})));
    #[cfg(any(
        feature = "middleware-jq",
        feature = "middleware-bundled-jq",
        feature = "middleware-all"
    ))]
    cases.push(("jq", json!({"Raw": {"insert": true}})));
    #[cfg(any(feature = "middleware-unwind", feature = "middleware-all"))]
    cases.push((
        "unwind",
        json!({"Parent": [{"selector": 7, "label": "Child"}]}),
    ));
    // Also reject bad definitions that are not referenced by the selected pipeline.
    cases.push((
        "fixture",
        json!({"operation": "add", "operand": "not a number"}),
    ));
    for (kind, invalid) in cases {
        assert_setup_error(vec![config(kind, "invalid", invalid)], &[], "config");
    }
}

#[tokio::test]
async fn custom_middleware_works_without_builtin_features() {
    assert_element_mutations(
        config("fixture", "add", json!({"operation": "add", "operand": 2})),
        json!({"value": 5, "untouched": "kept"}),
        json!({"value": 7, "untouched": "kept"}),
        "Raw",
        "Raw",
    )
    .await;
}

#[tokio::test]
async fn processing_requires_start() {
    let mut subject =
        MiddlewareTransformer::new(definition(Vec::new(), &[]), registry()).expect("construct");
    let input = event(
        "producer",
        40,
        SourceChange::Insert {
            element: node("database", "item", "Raw", 100, json!({"value": 5})),
        },
    );
    assert!(subject
        .transform(InputEnvelope {
            port: port("in"),
            envelope: input,
        })
        .await
        .is_err());
}

#[tokio::test]
async fn metadata_ancestry_and_input_are_preserved_across_clean_restart() {
    let mut root = event(
        "producer",
        40,
        SourceChange::Insert {
            element: node("database", "item", "Raw", 100, json!({"value": 5})),
        },
    );
    root.append_annotation(
        ContextEntry::try_new(
            component("producer"),
            "branch",
            ContextValue::String(Arc::from("second contribution")),
        )
        .expect("duplicate annotation key"),
    )
    .expect("append history");
    let input = derived_input(&root, "upstream", 91, root.changes().operations().to_vec());
    let original = decoded(&input);
    let annotations: Vec<_> = input.annotations().entries().collect();
    let mut subject = running(
        vec![config("fixture", "add", json!({"operation": "add", "operand": 2}))],
        &["add"],
    )
    .await;
    let first = transform_one(&mut subject, &input).await;
    assert_eq!(
        decoded(&first),
        [SourceChange::Insert {
            element: node("database", "item", "Raw", 100, json!({"value": 7})),
        }]
    );
    let metadata = GraphChangeCodec::source_metadata(&first)
        .expect("metadata")
        .expect("raw source metadata");
    assert_eq!(metadata.source_id, "raw-producer");
    assert_eq!(metadata.sequence, Some(1040));
    assert_eq!(metadata.source_position, Some(vec![0xff, 0, 7]));
    assert_eq!(
        metadata.profiling.expect("profiling").source_send_ns,
        Some(13)
    );
    assert_eq!(metadata.schema, Some(SourceSchema::default()));
    assert_eq!(decoded(&input), original);
    assert_eq!(
        input.annotations().entries().collect::<Vec<_>>(),
        annotations
    );

    subject.stop().await.expect("stop");
    subject.start().await.expect("restart same instance");
    let next_input = event(
        "producer",
        50,
        SourceChange::Update {
            element: node("database", "item", "Raw", 200, json!({"value": 8})),
        },
    );
    let second = transform_one(&mut subject, &next_input).await;
    assert!(second.system().sequence() > first.system().sequence());
    assert_ne!(second.id(), first.id());
    assert_eq!(
        decoded(&second),
        [SourceChange::Update {
            element: node("database", "item", "Raw", 200, json!({"value": 10})),
        }]
    );
    subject.stop().await.expect("stop");
}

#[tokio::test]
async fn empty_pipeline_preserves_batch_order_and_decodes_replace_as_delete_insert() {
    let replacement = node("database", "item", "Raw", 100, json!({"value": 4}));
    let patch = node("database", "item", "Raw", 200, json!({"value": 5}));
    let deletion = metadata("database", "item", "Raw", 300);
    let root = event(
        "producer",
        40,
        SourceChange::Insert {
            element: replacement.clone(),
        },
    );
    let delete_record = record(SourceChange::Delete {
        metadata: deletion.clone(),
    });
    let input = derived_input(
        &root,
        "upstream",
        90,
        vec![
            ChangeOperation::Updated {
                ordinal: 10,
                before: None,
                after: record(SourceChange::Insert {
                    element: replacement.clone(),
                }),
                semantics: UpdateSemantics::Replace,
            },
            ChangeOperation::Updated {
                ordinal: 20,
                before: None,
                after: record(SourceChange::Update {
                    element: patch.clone(),
                }),
                semantics: UpdateSemantics::Patch,
            },
            ChangeOperation::Deleted {
                ordinal: 30,
                identity: delete_record.reference().clone(),
                before: Some(delete_record),
            },
        ],
    );
    let mut subject = running(Vec::new(), &[]).await;
    let output = transform_one(&mut subject, &input).await;
    assert_eq!(output.changes().operations().len(), 4);
    assert_eq!(
        decoded(&output),
        [
            SourceChange::Delete {
                metadata: replacement.get_metadata().clone(),
            },
            SourceChange::Insert {
                element: replacement,
            },
            SourceChange::Update { element: patch },
            SourceChange::Delete { metadata: deletion },
        ]
    );
    assert!(matches!(
        &output.changes().operations()[2],
        ChangeOperation::Updated {
            semantics: UpdateSemantics::Patch,
            ..
        }
    ));
    assert_future_passthrough(&mut subject).await;
    subject.stop().await.expect("stop");
}

#[tokio::test]
async fn filtering_and_already_empty_input_still_emit_progress_batches() {
    let root = event(
        "producer",
        40,
        SourceChange::Insert {
            element: node("database", "item", "Raw", 100, json!({"value": 5})),
        },
    );
    let mut subject = running(
        vec![config("fixture", "filter", json!({"operation": "filter"}))],
        &["filter"],
    )
    .await;
    let filtered = transform_one(&mut subject, &root).await;
    assert!(filtered.changes().is_empty());
    assert!(decoded(&filtered).is_empty());
    let empty = derived_input(&root, "upstream", 91, Vec::new());
    let output = transform_one(&mut subject, &empty).await;
    assert!(output.changes().is_empty());
    assert!(output.system().sequence() > filtered.system().sequence());
    assert_ne!(output.id(), filtered.id());
    subject.stop().await.expect("stop");
}

#[tokio::test]
async fn a_later_record_failure_emits_no_partial_batch_and_fences_the_instance() {
    let valid_change = SourceChange::Insert {
        element: node("database", "good", "Raw", 100, json!({"value": 5})),
    };
    let valid = event("producer", 40, valid_change.clone());
    let invalid = derived_input(
        &valid,
        "upstream",
        91,
        vec![
            ChangeOperation::Added {
                ordinal: 0,
                after: record(valid_change),
            },
            ChangeOperation::Added {
                ordinal: 1,
                after: record(SourceChange::Insert {
                    element: node(
                        "database",
                        "bad",
                        "Raw",
                        100,
                        json!({"value": "not an integer"}),
                    ),
                }),
            },
        ],
    );
    let output = assert_processing_failure(
        config("fixture", "add", json!({"operation": "add", "operand": 2})),
        &invalid,
        &valid,
        "fixture requires an integer value",
    )
    .await;
    assert_eq!(
        decoded(&output),
        [SourceChange::Insert {
            element: node("database", "good", "Raw", 100, json!({"value": 7})),
        }]
    );
}

struct GraphSource {
    descriptor: ComponentDescriptor,
    events: VecDeque<ChangeEnvelope>,
    started: bool,
}

struct GraphCollector {
    descriptor: ComponentDescriptor,
    events: Arc<Mutex<Vec<ChangeEnvelope>>>,
    started: bool,
}

fn graph_descriptor(id: &str, name: &str, direction: PortDirection) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        component(id),
        vec![PortDescriptor::new(
            port(name),
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("graph component descriptor")
}

macro_rules! lifecycle {
    ($type:ty) => {
        #[async_trait]
        impl ComputationComponent for $type {
            fn descriptor(&self) -> &ComponentDescriptor {
                &self.descriptor
            }

            async fn start(&mut self) -> anyhow::Result<()> {
                self.started = true;
                Ok(())
            }

            async fn stop(&mut self) -> anyhow::Result<()> {
                self.started = false;
                Ok(())
            }
        }
    };
}

lifecycle!(GraphSource);
lifecycle!(GraphCollector);

#[async_trait]
impl EnvelopeSource for GraphSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        assert!(self.started);
        Ok(self.events.pop_front().map(|envelope| OutputEnvelope {
            port: port("out"),
            envelope,
        }))
    }
}

#[async_trait]
impl EnvelopeSink for GraphCollector {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        assert!(self.started);
        assert_eq!(input.port, port("in"));
        self.events.lock().expect("collector").push(input.envelope);
        Ok(())
    }
}

async fn two_producer_graph(
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: &[&str],
    expected_label: &str,
    expected_value: fn(i64) -> i64,
) {
    let inputs: Vec<_> = [
        ("left", "database-a", 40, 3, false),
        ("left", "database-a", 50, 5, true),
        ("right", "database-b", 7, 7, false),
        ("right", "database-b", 9, 11, true),
    ]
    .into_iter()
    .map(|(producer, source, sequence, value, update)| {
        event(
            producer,
            sequence,
            mutation(
                node(source, "same-local-id", "Raw", 100, json!({"value": value})),
                update,
            ),
        )
    })
    .collect();
    let original_changes: Vec<_> = inputs.iter().map(decoded).collect();
    let received = Arc::new(Mutex::new(Vec::new()));
    let subject = MiddlewareTransformer::new(definition(middleware, pipeline), registry())
        .expect("construct graph middleware");
    let mut graph = ComputationGraph::builder("middleware-without-continuous-query")
        .source(Box::new(GraphSource {
            descriptor: graph_descriptor("left", "out", PortDirection::Output),
            events: inputs[..2].iter().cloned().collect(),
            started: false,
        }))
        .source(Box::new(GraphSource {
            descriptor: graph_descriptor("right", "out", PortDirection::Output),
            events: inputs[2..].iter().cloned().collect(),
            started: false,
        }))
        .transformer(Box::new(subject))
        .sink(Box::new(GraphCollector {
            descriptor: graph_descriptor("collector", "in", PortDirection::Input),
            events: received.clone(),
            started: false,
        }))
        .bind_stream(endpoint("left", "out"), stream("left"))
        .bind_stream(endpoint("right", "out"), stream("right"))
        .bind_stream(endpoint("middleware", "out"), stream("middleware"))
        .connect(
            EdgeDefinition::new(endpoint("left", "out"), endpoint("middleware", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("right", "out"), endpoint("middleware", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("middleware", "out"), endpoint("collector", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("two producers may share the middleware input port");
    let run = graph.start().expect("graph starts");
    let control = run.control();
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("finite graph must not hang")
        .expect("finite graph drains");
    assert_eq!(control.state(), GraphState::Completed);
    assert_eq!(graph.state(), GraphState::Completed);

    let outputs = received.lock().expect("collector");
    assert_eq!(outputs.len(), 4);
    assert!(outputs
        .windows(2)
        .all(|pair| pair[0].system().sequence() < pair[1].system().sequence()));
    for (input, original) in inputs.iter().zip(original_changes) {
        let matches: Vec<_> = outputs
            .iter()
            .filter(|output| output.lineage().expect("lineage").envelope_id() == input.id())
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "neither lose nor duplicate either producer's input"
        );
        let output = matches[0];
        assert_derived(output, input);
        let (element, update) = match &original[0] {
            SourceChange::Insert { element } => (element, false),
            SourceChange::Update { element } => (element, true),
            _ => panic!("node mutation"),
        };
        let ElementValue::Integer(value) = element.get_property("value") else {
            panic!("integer value");
        };
        assert_eq!(
            decoded(output),
            [mutation(
                node(
                    &element.get_reference().source_id,
                    &element.get_reference().element_id,
                    expected_label,
                    element.get_effective_from(),
                    json!({"value": expected_value(*value)}),
                ),
                update,
            )]
        );
        assert_eq!(decoded(input), original);
    }
    for producer in ["left", "right"] {
        let input_sequences: Vec<_> = outputs
            .iter()
            .filter_map(|output| {
                let lineage = output.lineage().expect("lineage");
                (lineage.system().stream() == &stream(producer))
                    .then_some(lineage.system().sequence())
            })
            .collect();
        assert_eq!(
            input_sequences,
            if producer == "left" {
                vec![40, 50]
            } else {
                vec![7, 9]
            }
        );
    }
}

async fn custom_graph_scenario() {
    two_producer_graph(
        vec![
            config(
                "fixture",
                "multiply",
                json!({"operation": "multiply", "operand": 3}),
            ),
            config("fixture", "add", json!({"operation": "add", "operand": 2})),
        ],
        &["add", "multiply"],
        "Raw",
        |value| (value + 2) * 3,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn two_producer_graph_current_thread() {
    custom_graph_scenario().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_producer_graph_multi_thread() {
    custom_graph_scenario().await;
}

#[cfg(any(feature = "middleware-decoder", feature = "middleware-all"))]
mod decoder {
    use super::*;

    fn settings() -> Value {
        json!({
            "encoding_type": "base64",
            "target_property": "encoded",
            "output_property": "decoded"
        })
    }

    #[tokio::test]
    async fn decoder_handles_insert_update_delete_and_future() {
        let mut settings = settings();
        settings["on_error"] = json!("fail");
        assert_element_mutations(
            config("decoder", "decode", settings),
            json!({"encoded": "aGVsbG8=", "untouched": 7}),
            json!({"encoded": "aGVsbG8=", "decoded": "hello", "untouched": 7}),
            "Raw",
            "Raw",
        )
        .await;
    }

    #[tokio::test]
    async fn decoder_default_errors_are_not_silently_filtered_or_passed_through() {
        let valid = event(
            "producer",
            50,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"encoded": "aGVsbG8="}),
                ),
            },
        );
        for properties in [json!({}), json!({"encoded": 7}), json!({"encoded": "%%%"})] {
            let invalid = event(
                "producer",
                40,
                SourceChange::Insert {
                    element: node("database", "item", "Raw", 100, properties),
                },
            );
            let output = assert_processing_failure(
                config("decoder", "decode", settings()),
                &invalid,
                &valid,
                "encoded",
            )
            .await;
            assert_eq!(
                decoded(&output),
                [SourceChange::Insert {
                    element: node(
                        "database",
                        "item",
                        "Raw",
                        100,
                        json!({"encoded": "aGVsbG8=", "decoded": "hello"}),
                    ),
                }]
            );
        }
    }

    #[tokio::test]
    async fn decoder_skip_is_success_and_does_not_fence_later_input() {
        let mut settings = settings();
        settings["on_error"] = json!("skip");
        let mut subject = running(vec![config("decoder", "decode", settings)], &["decode"]).await;
        let invalid = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node("database", "item", "Raw", 100, json!({"encoded": "%%%"})),
            },
        );
        let skipped = transform_one(&mut subject, &invalid).await;
        assert_eq!(decoded(&skipped), decoded(&invalid));
        let valid = event(
            "producer",
            50,
            SourceChange::Update {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    200,
                    json!({"encoded": "aGVsbG8="}),
                ),
            },
        );
        let output = transform_one(&mut subject, &valid).await;
        assert!(output.system().sequence() > skipped.system().sequence());
        assert_eq!(
            decoded(&output),
            [SourceChange::Update {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    200,
                    json!({"encoded": "aGVsbG8=", "decoded": "hello"}),
                ),
            }]
        );
        subject.stop().await.expect("stop");
    }
}

#[cfg(any(feature = "middleware-parse-json", feature = "middleware-all"))]
mod parse_json {
    use super::*;

    fn settings() -> Value {
        json!({"target_property": "decoded", "output_property": "parsed"})
    }

    #[tokio::test]
    async fn parse_json_handles_insert_update_delete_and_future() {
        let text = r#"{"value":42,"items":[true,null,"x"]}"#;
        let mut settings = settings();
        settings["on_error"] = json!("fail");
        assert_element_mutations(
            config("parse_json", "parse", settings),
            json!({"decoded": text, "untouched": 7}),
            json!({
                "decoded": text,
                "parsed": {"value": 42, "items": [true, null, "x"]},
                "untouched": 7
            }),
            "Raw",
            "Raw",
        )
        .await;
    }

    #[tokio::test]
    async fn parse_json_default_errors_are_not_silently_filtered_or_passed_through() {
        let valid = event(
            "producer",
            50,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"decoded": "{\"value\":42}"}),
                ),
            },
        );
        for properties in [json!({}), json!({"decoded": 7}), json!({"decoded": "{invalid"})] {
            let invalid = event(
                "producer",
                40,
                SourceChange::Insert {
                    element: node("database", "item", "Raw", 100, properties),
                },
            );
            let output = assert_processing_failure(
                config("parse_json", "parse", settings()),
                &invalid,
                &valid,
                "decoded",
            )
            .await;
            assert_eq!(
                decoded(&output),
                [SourceChange::Insert {
                    element: node(
                        "database",
                        "item",
                        "Raw",
                        100,
                        json!({"decoded": "{\"value\":42}", "parsed": {"value": 42}}),
                    ),
                }]
            );
        }
    }

    #[tokio::test]
    async fn parse_json_skip_is_success_and_does_not_fence_later_input() {
        let mut settings = settings();
        settings["on_error"] = json!("skip");
        let mut subject = running(vec![config("parse_json", "parse", settings)], &["parse"]).await;
        let invalid = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"decoded": "{invalid"}),
                ),
            },
        );
        let skipped = transform_one(&mut subject, &invalid).await;
        assert_eq!(decoded(&skipped), decoded(&invalid));
        let valid = event(
            "producer",
            50,
            SourceChange::Update {
                element: node("database", "item", "Raw", 200, json!({"decoded": "[1,2]"})),
            },
        );
        let output = transform_one(&mut subject, &valid).await;
        assert!(output.system().sequence() > skipped.system().sequence());
        assert_eq!(
            decoded(&output),
            [SourceChange::Update {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    200,
                    json!({"decoded": "[1,2]", "parsed": [1, 2]}),
                ),
            }]
        );
        subject.stop().await.expect("stop");
    }
}

#[cfg(any(feature = "middleware-promote", feature = "middleware-all"))]
mod promote {
    use super::*;

    fn settings() -> Value {
        json!({"mappings": [{"path": "$.parsed.value", "target_name": "value"}]})
    }

    #[tokio::test]
    async fn promote_handles_insert_update_delete_and_future() {
        let mut settings = settings();
        settings["on_error"] = json!("fail");
        settings["on_conflict"] = json!("overwrite");
        assert_element_mutations(
            config("promote", "promote", settings),
            json!({"parsed": {"value": 42}, "value": -1, "untouched": 7}),
            json!({"parsed": {"value": 42}, "value": 42, "untouched": 7}),
            "Raw",
            "Raw",
        )
        .await;
    }

    #[tokio::test]
    async fn promote_default_missing_and_multiple_selection_errors_fail() {
        for (path, bad, good) in [
            (
                "$.parsed.value",
                json!({}),
                json!({"parsed": {"value": 42}}),
            ),
            (
                "$.items[*]",
                json!({"items": [1, 2]}),
                json!({"items": [42]}),
            ),
        ] {
            let invalid = event(
                "producer",
                40,
                SourceChange::Insert {
                    element: node("database", "item", "Raw", 100, bad),
                },
            );
            let valid = event(
                "producer",
                50,
                SourceChange::Insert {
                    element: node("database", "item", "Raw", 100, good.clone()),
                },
            );
            let output = assert_processing_failure(
                config(
                    "promote",
                    "promote",
                    json!({"mappings": [{"path": path, "target_name": "value"}]}),
                ),
                &invalid,
                &valid,
                "JSONPath",
            )
            .await;
            let mut expected = good;
            expected["value"] = json!(42);
            assert_eq!(
                decoded(&output),
                [SourceChange::Insert {
                    element: node("database", "item", "Raw", 100, expected),
                }]
            );
        }
    }

    #[tokio::test]
    async fn promote_explicit_conflict_failure_is_not_ignored() {
        let mut settings = settings();
        settings["on_conflict"] = json!("fail");
        let invalid = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"parsed": {"value": 42}, "value": 9}),
                ),
            },
        );
        let valid = event(
            "producer",
            50,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"parsed": {"value": 42}}),
                ),
            },
        );
        let output = assert_processing_failure(
            config("promote", "promote", settings),
            &invalid,
            &valid,
            "already exists",
        )
        .await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"parsed": {"value": 42}, "value": 42}),
                ),
            }]
        );
    }

    #[tokio::test]
    async fn promote_skip_is_success_and_does_not_fence_later_input() {
        let mut settings = settings();
        settings["on_error"] = json!("skip");
        let mut subject = running(vec![config("promote", "promote", settings)], &["promote"]).await;
        let invalid = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node("database", "item", "Raw", 100, json!({"untouched": 7})),
            },
        );
        let skipped = transform_one(&mut subject, &invalid).await;
        assert_eq!(decoded(&skipped), decoded(&invalid));
        let valid = event(
            "producer",
            50,
            SourceChange::Update {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    200,
                    json!({"parsed": {"value": 42}}),
                ),
            },
        );
        let output = transform_one(&mut subject, &valid).await;
        assert!(output.system().sequence() > skipped.system().sequence());
        assert_eq!(
            decoded(&output),
            [SourceChange::Update {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    200,
                    json!({"parsed": {"value": 42}, "value": 42}),
                ),
            }]
        );
        subject.stop().await.expect("stop");
    }
}

#[cfg(any(feature = "middleware-relabel", feature = "middleware-all"))]
mod relabel {
    use super::*;

    #[tokio::test]
    async fn relabel_handles_insert_update_delete_and_future() {
        assert_element_mutations(
            config(
                "relabel",
                "label",
                json!({"labelMappings": {"Raw": "Reading"}}),
            ),
            json!({"value": 42}),
            json!({"value": 42}),
            "Raw",
            "Reading",
        )
        .await;
    }

    #[tokio::test]
    async fn relabel_preserves_unmapped_labels_and_relation_endpoints() {
        let mut subject = running(
            vec![config(
                "relabel",
                "label",
                json!({"labelMappings": {"Raw": "Reading"}}),
            )],
            &["label"],
        )
        .await;
        let mut original = metadata("database", "edge", "Raw", 100);
        original.labels = Arc::from([Arc::from("Raw"), Arc::from("Keep")]);
        let mut expected = original.clone();
        expected.labels = Arc::from([Arc::from("Reading"), Arc::from("Keep")]);
        let relation = Element::Relation {
            metadata: original,
            in_node: ElementReference::new("database", "from"),
            out_node: ElementReference::new("database", "to"),
            properties: ElementPropertyMap::from(json!({"weight": 42})),
        };
        let input = event("producer", 40, SourceChange::Insert { element: relation });
        let output = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Insert {
                element: Element::Relation {
                    metadata: expected,
                    in_node: ElementReference::new("database", "from"),
                    out_node: ElementReference::new("database", "to"),
                    properties: ElementPropertyMap::from(json!({"weight": 42})),
                },
            }]
        );
        subject.stop().await.expect("stop");
    }
}

#[cfg(any(feature = "middleware-map", feature = "middleware-all"))]
mod map {
    use super::*;

    #[tokio::test]
    async fn map_expands_in_order_changes_ids_operations_and_endpoints_and_filters() {
        let mut subject = running(
            vec![config(
                "map",
                "project",
                json!({
                    "Reading": {
                        "insert": [
                            {"id": "$.first_id", "label": "Mapped", "properties": {"value": "$.value"}},
                            {"op": "Update", "id": "$.second_id", "label": "Patched", "properties": {"value": "$.other"}},
                            {
                                "id": "$.edge_id",
                                "label": "LINKS",
                                "elementType": {"Relation": {"inNodeId": "$.first_id", "outNodeId": "$.second_id"}},
                                "properties": {"weight": "$.weight"}
                            }
                        ],
                        "update": [{"label": "Mapped", "properties": {"value": "$.value"}}],
                        "delete": [{"label": "Gone"}]
                    }
                }),
            )],
            &["project"],
        )
        .await;
        let input = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Reading",
                    100,
                    json!({"first_id": "first", "second_id": "second", "edge_id": "edge", "value": 3, "other": 9, "weight": 2}),
                ),
            },
        );
        let expanded = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&expanded),
            [
                SourceChange::Insert {
                    element: node("database", "first", "Mapped", 100, json!({"value": 3})),
                },
                SourceChange::Update {
                    element: node("database", "second", "Patched", 100, json!({"value": 9})),
                },
                SourceChange::Insert {
                    element: Element::Relation {
                        metadata: metadata("database", "edge", "LINKS", 100),
                        in_node: ElementReference::new("database", "first"),
                        out_node: ElementReference::new("database", "second"),
                        properties: ElementPropertyMap::from(json!({"weight": 2})),
                    },
                },
            ]
        );
        let update = event(
            "producer",
            50,
            SourceChange::Update {
                element: node("database", "item", "Reading", 200, json!({"value": 11})),
            },
        );
        let output = transform_one(&mut subject, &update).await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Update {
                element: node("database", "item", "Mapped", 200, json!({"value": 11})),
            }]
        );
        assert!(output.system().sequence() > expanded.system().sequence());
        let delete = event(
            "producer",
            60,
            SourceChange::Delete {
                metadata: metadata("database", "item", "Reading", 300),
            },
        );
        let output = transform_one(&mut subject, &delete).await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Delete {
                metadata: metadata("database", "item", "Gone", 300),
            }]
        );
        let unmatched = event(
            "producer",
            70,
            SourceChange::Insert {
                element: node(
                    "database",
                    "filtered",
                    "Unmapped",
                    400,
                    json!({"value": 12}),
                ),
            },
        );
        let filtered = transform_one(&mut subject, &unmatched).await;
        assert!(filtered.changes().is_empty());
        assert!(filtered.system().sequence() > output.system().sequence());
        assert_future_passthrough(&mut subject).await;
        subject.stop().await.expect("stop");
    }
}

#[cfg(any(
    feature = "middleware-jq",
    feature = "middleware-bundled-jq",
    feature = "middleware-all"
))]
mod jq {
    use super::*;

    #[tokio::test]
    async fn jq_expands_arrays_in_order_maps_update_delete_and_filters_empty_arrays() {
        let mut subject = running(
            vec![config(
                "jq",
                "jq",
                json!({
                    "Reading": {
                        "insert": [{
                            "query": "[.items[] | {id: .id, value: (.value * 2)}]",
                            "id": ".id",
                            "label": "\"Mapped\"",
                            "haltOnError": true
                        }],
                        "update": [{"query": "{value: .value + 1}", "label": "\"Mapped\"", "haltOnError": true}],
                        "delete": [{"query": "{}", "label": "\"Gone\"", "haltOnError": true}]
                    }
                }),
            )],
            &["jq"],
        )
        .await;
        let input = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Reading",
                    100,
                    json!({"items": [{"id": "first", "value": 3}, {"id": "second", "value": 7}]}),
                ),
            },
        );
        let expanded = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&expanded),
            [
                SourceChange::Insert {
                    element: node(
                        "database",
                        "first",
                        "Mapped",
                        100,
                        json!({"id": "first", "value": 6})
                    ),
                },
                SourceChange::Insert {
                    element: node(
                        "database",
                        "second",
                        "Mapped",
                        100,
                        json!({"id": "second", "value": 14})
                    ),
                },
            ]
        );
        let update = event(
            "producer",
            50,
            SourceChange::Update {
                element: node("database", "item", "Reading", 200, json!({"value": 4})),
            },
        );
        let output = transform_one(&mut subject, &update).await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Update {
                element: node("database", "item", "Mapped", 200, json!({"value": 5})),
            }]
        );
        assert!(output.system().sequence() > expanded.system().sequence());
        let delete = event(
            "producer",
            60,
            SourceChange::Delete {
                metadata: metadata("database", "item", "Reading", 300),
            },
        );
        let output = transform_one(&mut subject, &delete).await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Delete {
                metadata: metadata("database", "item", "Gone", 300),
            }]
        );
        let empty = event(
            "producer",
            70,
            SourceChange::Insert {
                element: node("database", "empty", "Reading", 400, json!({"items": []})),
            },
        );
        let filtered = transform_one(&mut subject, &empty).await;
        assert!(filtered.changes().is_empty());
        assert!(filtered.system().sequence() > output.system().sequence());
        assert_future_passthrough(&mut subject).await;
        subject.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn jq_halt_on_error_fails_instead_of_silently_dropping_a_record() {
        let invalid = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node("database", "item", "Reading", 100, json!({"fail": true})),
            },
        );
        let valid = event(
            "producer",
            50,
            SourceChange::Insert {
                element: node("database", "item", "Reading", 100, json!({"value": 7})),
            },
        );
        let output = assert_processing_failure(
            config(
                "jq",
                "jq",
                json!({"Reading": {"insert": [{
                    "query": "if .fail then error(\"jq fixture failure\") else {value: .value} end",
                    "label": "\"Mapped\"",
                    "haltOnError": true
                }]}}),
            ),
            &invalid,
            &valid,
            "JQ",
        )
        .await;
        assert_eq!(
            decoded(&output),
            [SourceChange::Insert {
                element: node("database", "item", "Mapped", 100, json!({"value": 7})),
            }]
        );
    }
}

#[cfg(any(feature = "middleware-unwind", feature = "middleware-all"))]
mod unwind {
    use super::*;

    fn settings() -> SourceMiddlewareConfig {
        config(
            "unwind",
            "children",
            json!({"Parent": [{"selector": "$.items[*]", "label": "Child", "key": "$.id", "relation": "OWNS"}]}),
        )
    }

    fn child(id: &str, time: u64, value: i64) -> Element {
        node(
            "database",
            &format!("$unwind-$.items[*]-parent-{id}"),
            "Child",
            time,
            json!({"id": id, "value": value}),
        )
    }

    fn relation(id: &str, time: u64) -> Element {
        Element::Relation {
            metadata: metadata(
                "database",
                &format!("$unwind-$.items[*]-parent-{id}$rel"),
                "OWNS",
                time,
            ),
            in_node: ElementReference::new("database", "parent"),
            out_node: child(id, time, 0).get_reference().clone(),
            properties: ElementPropertyMap::new(),
        }
    }

    fn assert_changes(actual: &[SourceChange], expected: Vec<SourceChange>) {
        assert_eq!(actual.len(), expected.len());
        for change in &expected {
            assert!(actual.contains(change), "missing {change:?} in {actual:?}");
        }
        assert_eq!(
            actual.last(),
            expected.last(),
            "Unwind emits the parent last"
        );
        // Unwind's child groups come from a HashMap; only each child/relation pair is ordered.
        for pair in actual[..actual.len() - 1].chunks_exact(2) {
            assert_eq!(
                pair[1].get_reference().element_id.as_ref(),
                format!("{}$rel", pair[0].get_reference().element_id)
            );
        }
    }

    #[tokio::test]
    async fn unwind_shrinking_update_and_delete_cleanup_use_owned_state_across_restart() {
        let original = node(
            "database",
            "parent",
            "Parent",
            100,
            json!({"items": [{"id": "a", "value": 10}, {"id": "b", "value": 20}]}),
        );
        let mut subject = running(vec![settings()], &["children"]).await;
        let input = event(
            "producer",
            40,
            SourceChange::Insert {
                element: original.clone(),
            },
        );
        let inserted = transform_one(&mut subject, &input).await;
        assert_changes(
            &decoded(&inserted),
            vec![
                SourceChange::Insert {
                    element: child("a", 100, 10),
                },
                SourceChange::Insert {
                    element: relation("a", 100),
                },
                SourceChange::Insert {
                    element: child("b", 100, 20),
                },
                SourceChange::Insert {
                    element: relation("b", 100),
                },
                SourceChange::Insert { element: original },
            ],
        );

        subject.stop().await.expect("stop after insert");
        subject
            .start()
            .await
            .expect("restart retains emitted element state");
        let smaller = node(
            "database",
            "parent",
            "Parent",
            200,
            json!({"items": [{"id": "b", "value": 21}]}),
        );
        let update = event(
            "producer",
            50,
            SourceChange::Update {
                element: smaller.clone(),
            },
        );
        let updated = transform_one(&mut subject, &update).await;
        assert_changes(
            &decoded(&updated),
            vec![
                SourceChange::Delete {
                    metadata: child("a", 200, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("a", 200).get_metadata().clone(),
                },
                SourceChange::Update {
                    element: child("b", 200, 21),
                },
                SourceChange::Update {
                    element: relation("b", 200),
                },
                SourceChange::Update { element: smaller },
            ],
        );
        assert!(updated.system().sequence() > inserted.system().sequence());

        subject.stop().await.expect("stop after update");
        subject
            .start()
            .await
            .expect("restart retains shrinking update");
        let deletion = SourceChange::Delete {
            metadata: metadata("database", "parent", "Parent", 300),
        };
        let delete = event("producer", 60, deletion.clone());
        let deleted = transform_one(&mut subject, &delete).await;
        assert_changes(
            &decoded(&deleted),
            vec![
                SourceChange::Delete {
                    metadata: child("b", 300, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("b", 300).get_metadata().clone(),
                },
                deletion,
            ],
        );
        assert!(deleted.system().sequence() > updated.system().sequence());
        let repeated_delete = event(
            "producer",
            70,
            SourceChange::Delete {
                metadata: metadata("database", "parent", "Parent", 400),
            },
        );
        let repeated = transform_one(&mut subject, &repeated_delete).await;
        assert_eq!(decoded(&repeated), decoded(&repeated_delete));
        assert_eq!(
            repeated.changes().operations().len(),
            1,
            "deleted state is removed"
        );
        assert_future_passthrough(&mut subject).await;
        subject.stop().await.expect("stop");

        let mut reconstructed = running(vec![settings()], &["children"]).await;
        let output = transform_one(&mut reconstructed, &delete).await;
        assert_eq!(
            decoded(&output),
            decoded(&delete),
            "new instances have no durable prior state"
        );
        reconstructed.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn unwind_remembers_prior_operations_inside_one_input_batch() {
        let insert = SourceChange::Insert {
            element: node(
                "database",
                "parent",
                "Parent",
                100,
                json!({"items": [{"id": "a", "value": 10}, {"id": "b", "value": 20}]}),
            ),
        };
        let update = SourceChange::Update {
            element: node(
                "database",
                "parent",
                "Parent",
                200,
                json!({"items": [{"id": "b", "value": 21}]}),
            ),
        };
        let delete = SourceChange::Delete {
            metadata: metadata("database", "parent", "Parent", 300),
        };
        let root = event("producer", 40, insert.clone());
        let original = [insert.clone(), update.clone(), delete.clone()];
        let input = GraphChangeCodec::derive_changes(&root, &original, stream("batch"), 90)
            .expect("encode a metadata-preserving graph batch");
        let mut subject = running(vec![settings()], &["children"]).await;
        let output = transform_one(&mut subject, &input).await;
        let changes = decoded(&output);
        assert_eq!(changes.len(), 13, "insert 5, shrinking update 5, delete 3");
        assert_changes(
            &changes[..5],
            vec![
                SourceChange::Insert {
                    element: child("a", 100, 10),
                },
                SourceChange::Insert {
                    element: relation("a", 100),
                },
                SourceChange::Insert {
                    element: child("b", 100, 20),
                },
                SourceChange::Insert {
                    element: relation("b", 100),
                },
                insert,
            ],
        );
        assert_changes(
            &changes[5..10],
            vec![
                SourceChange::Delete {
                    metadata: child("a", 200, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("a", 200).get_metadata().clone(),
                },
                SourceChange::Update {
                    element: child("b", 200, 21),
                },
                SourceChange::Update {
                    element: relation("b", 200),
                },
                update,
            ],
        );
        assert_changes(
            &changes[10..],
            vec![
                SourceChange::Delete {
                    metadata: child("b", 300, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("b", 300).get_metadata().clone(),
                },
                delete,
            ],
        );
        assert_eq!(decoded(&input), original);
        let repeated = event(
            "producer",
            50,
            SourceChange::Delete {
                metadata: metadata("database", "parent", "Parent", 400),
            },
        );
        let next = transform_one(&mut subject, &repeated).await;
        assert_eq!(decoded(&next), decoded(&repeated));
        assert!(next.system().sequence() > output.system().sequence());
        subject.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn unwind_full_replacement_with_new_label_cleans_old_label_children_first() {
        let original = node(
            "database",
            "parent",
            "Parent",
            100,
            json!({"items": [{"id": "a", "value": 10}]}),
        );
        let initial = SourceChange::Insert {
            element: original.clone(),
        };
        let mut subject = running(vec![settings()], &["children"]).await;
        let inserted = transform_one(&mut subject, &event("producer", 40, initial.clone())).await;
        assert_eq!(
            decoded(&inserted),
            [
                SourceChange::Insert {
                    element: child("a", 100, 10),
                },
                SourceChange::Insert {
                    element: relation("a", 100),
                },
                initial,
            ]
        );

        let replacement = node(
            "database",
            "parent",
            "Archived",
            200,
            json!({"status": "archived"}),
        );
        let root = event(
            "producer",
            50,
            SourceChange::Insert {
                element: replacement.clone(),
            },
        );
        let input = derived_input(
            &root,
            "replacement",
            90,
            vec![ChangeOperation::Updated {
                ordinal: 0,
                before: Some(record(SourceChange::Insert { element: original })),
                after: record(SourceChange::Insert {
                    element: replacement.clone(),
                }),
                semantics: UpdateSemantics::Replace,
            }],
        );
        let replaced = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&replaced),
            [
                SourceChange::Delete {
                    metadata: child("a", 200, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("a", 200).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: metadata("database", "parent", "Parent", 200),
                },
                SourceChange::Insert {
                    element: replacement,
                },
            ],
            "the before-record's Parent label must drive cleanup at the replacement time"
        );
        assert!(replaced.system().sequence() > inserted.system().sequence());

        let deletion = SourceChange::Delete {
            metadata: metadata("database", "parent", "Archived", 300),
        };
        let deleted = transform_one(&mut subject, &event("producer", 60, deletion.clone())).await;
        assert_eq!(decoded(&deleted), [deletion]);
        assert!(deleted.system().sequence() > replaced.system().sequence());
        subject.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn unwind_update_without_selected_array_deletes_children_before_property_merge() {
        let insert = SourceChange::Insert {
            element: node(
                "database",
                "parent",
                "Parent",
                100,
                json!({"items": [{"id": "a", "value": 10}], "status": "original"}),
            ),
        };
        let update = SourceChange::Update {
            element: node(
                "database",
                "parent",
                "Parent",
                200,
                json!({"status": "changed"}),
            ),
        };
        let delete = SourceChange::Delete {
            metadata: metadata("database", "parent", "Parent", 300),
        };
        let mut subject = running(vec![settings()], &["children"]).await;
        let inserted = transform_one(&mut subject, &event("producer", 40, insert.clone())).await;
        assert_eq!(
            decoded(&inserted),
            [
                SourceChange::Insert {
                    element: child("a", 100, 10),
                },
                SourceChange::Insert {
                    element: relation("a", 100),
                },
                insert,
            ]
        );

        let input = event("producer", 50, update.clone());
        let updated = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&updated),
            [
                SourceChange::Delete {
                    metadata: child("a", 200, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("a", 200).get_metadata().clone(),
                },
                update,
            ],
            "omitting items selects no children; the outgoing parent remains a partial patch"
        );
        assert!(updated.system().sequence() > inserted.system().sequence());

        // The final parent patch is merged only after middleware. Its saved items
        // therefore remain available, so a later Delete repeats their tombstones.
        let deleted = transform_one(&mut subject, &event("producer", 60, delete.clone())).await;
        assert_eq!(
            decoded(&deleted),
            [
                SourceChange::Delete {
                    metadata: child("a", 300, 0).get_metadata().clone(),
                },
                SourceChange::Delete {
                    metadata: relation("a", 300).get_metadata().clone(),
                },
                delete,
            ]
        );
        assert!(deleted.system().sequence() > updated.system().sequence());
        subject.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn two_unwind_steps_expand_and_clean_nested_arrays_using_final_emitted_state() {
        let group_id = "$unwind-$.groups[*]-parent-group";
        let leaf_id = |id: &str| format!("$unwind-$.items[*]-{group_id}-{id}");
        let parent = |time, items: Value| {
            node(
                "database",
                "parent",
                "Parent",
                time,
                json!({"groups": [{"id": "group", "items": items}]}),
            )
        };
        let group = |time, items: Value| {
            node(
                "database",
                group_id,
                "Group",
                time,
                json!({"id": "group", "items": items}),
            )
        };
        let leaf = |id: &str, time, value| {
            node(
                "database",
                &leaf_id(id),
                "Leaf",
                time,
                json!({"id": id, "value": value}),
            )
        };
        let group_relation = |time| Element::Relation {
            metadata: metadata("database", &format!("{group_id}$rel"), "OWNS_GROUP", time),
            in_node: ElementReference::new("database", "parent"),
            out_node: ElementReference::new("database", group_id),
            properties: ElementPropertyMap::new(),
        };
        let leaf_relation = |id: &str, time| Element::Relation {
            metadata: metadata(
                "database",
                &format!("{}$rel", leaf_id(id)),
                "OWNS_LEAF",
                time,
            ),
            in_node: ElementReference::new("database", group_id),
            out_node: ElementReference::new("database", &leaf_id(id)),
            properties: ElementPropertyMap::new(),
        };
        let mut subject = running(
            vec![
                config(
                    "unwind",
                    "leaves",
                    json!({"Group": [{
                        "selector": "$.items[*]",
                        "label": "Leaf",
                        "key": "$.id",
                        "relation": "OWNS_LEAF"
                    }]}),
                ),
                config(
                    "unwind",
                    "groups",
                    json!({"Parent": [{
                        "selector": "$.groups[*]",
                        "label": "Group",
                        "key": "$.id",
                        "relation": "OWNS_GROUP"
                    }]}),
                ),
            ],
            &["groups", "leaves"],
        )
        .await;
        let first_items = json!([{"id": "a", "value": 10}]);
        let next_items = json!([{"id": "b", "value": 20}]);
        let delete = |element: Element| SourceChange::Delete {
            metadata: element.get_metadata().clone(),
        };
        let cases = [
            (
                "insert both levels",
                mutation(parent(100, first_items.clone()), false),
                vec![
                    mutation(leaf("a", 100, 10), false),
                    mutation(leaf_relation("a", 100), false),
                    mutation(group(100, first_items.clone()), false),
                    mutation(group_relation(100), false),
                    mutation(parent(100, first_items), false),
                ],
            ),
            (
                "shrinking nested array deletes the previous leaf",
                mutation(parent(200, json!([])), true),
                vec![
                    delete(leaf("a", 200, 0)),
                    delete(leaf_relation("a", 200)),
                    mutation(group(200, json!([])), true),
                    mutation(group_relation(200), true),
                    mutation(parent(200, json!([])), true),
                ],
            ),
            (
                "expanding again uses the saved empty array, not stale per-step input",
                mutation(parent(300, next_items.clone()), true),
                vec![
                    mutation(leaf("b", 300, 20), true),
                    mutation(leaf_relation("b", 300), true),
                    mutation(group(300, next_items.clone()), true),
                    mutation(group_relation(300), true),
                    mutation(parent(300, next_items), true),
                ],
            ),
            (
                "deleting the parent cleans descendants before their parents",
                delete(parent(400, json!([]))),
                vec![
                    delete(leaf("b", 400, 0)),
                    delete(leaf_relation("b", 400)),
                    delete(group(400, json!([]))),
                    delete(group_relation(400)),
                    delete(parent(400, json!([]))),
                ],
            ),
            (
                "repeated delete has no descendants left",
                delete(parent(500, json!([]))),
                vec![delete(parent(500, json!([])))],
            ),
        ];
        let mut previous_sequence = None;
        for (index, (phase, change, expected)) in cases.into_iter().enumerate() {
            let input = event("producer", 40 + index as u64, change);
            let output = transform_one(&mut subject, &input).await;
            assert_eq!(decoded(&output), expected, "{phase}");
            assert_eq!(
                output.changes().operations().len(),
                [5, 5, 5, 5, 1][index],
                "{phase}"
            );
            if let Some(previous) = previous_sequence {
                assert!(output.system().sequence() > previous, "{phase}");
            }
            previous_sequence = Some(output.system().sequence());
        }
        subject.stop().await.expect("stop");
    }
}

#[cfg(all(
    any(feature = "middleware-decoder", feature = "middleware-all"),
    any(feature = "middleware-parse-json", feature = "middleware-all"),
    any(feature = "middleware-promote", feature = "middleware-all"),
    any(feature = "middleware-relabel", feature = "middleware-all")
))]
#[tokio::test]
async fn ordered_decoder_parse_json_promote_relabel_chain_uses_configured_names() {
    let middleware = vec![
        config(
            "relabel",
            "label",
            json!({"labelMappings": {"Raw": "Reading"}}),
        ),
        config(
            "promote",
            "promote",
            json!({
                "mappings": [{"path": "$.parsed.value", "target_name": "value"}],
                "on_conflict": "overwrite",
                "on_error": "fail"
            }),
        ),
        config(
            "decoder",
            "decode",
            json!({
                "encoding_type": "base64",
                "target_property": "encoded",
                "output_property": "decoded",
                "on_error": "fail"
            }),
        ),
        config(
            "parse_json",
            "parse",
            json!({
                "target_property": "decoded",
                "output_property": "parsed",
                "on_error": "fail"
            }),
        ),
    ];
    let input = event(
        "producer",
        40,
        SourceChange::Insert {
            element: node(
                "database",
                "item",
                "Raw",
                100,
                json!({"encoded": "eyJ2YWx1ZSI6NDJ9", "value": -1}),
            ),
        },
    );
    let mut subject = running(middleware.clone(), &["decode", "parse", "promote", "label"]).await;
    let output = transform_one(&mut subject, &input).await;
    assert_eq!(
        decoded(&output),
        [SourceChange::Insert {
            element: node(
                "database",
                "item",
                "Reading",
                100,
                json!({
                    "encoded": "eyJ2YWx1ZSI6NDJ9",
                    "decoded": "{\"value\":42}",
                    "parsed": {"value": 42},
                    "value": 42
                })
            ),
        }]
    );
    assert_eq!(
        decoded(&input)[0].get_reference(),
        &ElementReference::new("database", "item")
    );
    subject.stop().await.expect("stop");

    let mut wrong_order = running(middleware, &["promote", "parse", "decode", "label"]).await;
    let error = wrong_order
        .transform(InputEnvelope {
            port: port("in"),
            envelope: input,
        })
        .await
        .expect_err("promote cannot read parsed JSON before decoding and parsing");
    assert!(format!("{error:#}").contains("JSONPath"));
    wrong_order.stop().await.expect("stop failed chain");
}

#[cfg(all(
    any(feature = "middleware-relabel", feature = "middleware-all"),
    any(feature = "middleware-map", feature = "middleware-all")
))]
mod ordered_mapping {
    use super::*;

    #[tokio::test]
    async fn relabel_map_relabel_chain_transforms_every_expanded_record_in_order() {
        let middleware = vec![
            config(
                "map",
                "project",
                json!({"Reading": {"insert": [
                    {"id": "$.left", "label": "Mapped", "properties": {"value": "$.value"}},
                    {"id": "$.right", "label": "Mapped", "properties": {"value": "$.value"}}
                ]}}),
            ),
            config(
                "relabel",
                "after",
                json!({"labelMappings": {"Mapped": "Final"}}),
            ),
            config(
                "relabel",
                "before",
                json!({"labelMappings": {"Raw": "Reading"}}),
            ),
        ];
        let input = event(
            "producer",
            40,
            SourceChange::Insert {
                element: node(
                    "database",
                    "item",
                    "Raw",
                    100,
                    json!({"left": "first", "right": "second", "value": 8}),
                ),
            },
        );
        let mut subject = running(middleware.clone(), &["before", "project", "after"]).await;
        let output = transform_one(&mut subject, &input).await;
        assert_eq!(
            decoded(&output),
            [
                SourceChange::Insert {
                    element: node("database", "first", "Final", 100, json!({"value": 8})),
                },
                SourceChange::Insert {
                    element: node("database", "second", "Final", 100, json!({"value": 8})),
                },
            ]
        );
        subject.stop().await.expect("stop");

        let mut wrong_order = running(middleware, &["project", "before", "after"]).await;
        let output = transform_one(&mut wrong_order, &input).await;
        assert!(
            output.changes().is_empty(),
            "map sees Raw before it is relabeled to Reading"
        );
        wrong_order.stop().await.expect("stop");
    }

    async fn graph_scenario() {
        two_producer_graph(
            vec![
                config(
                    "map",
                    "project",
                    json!({"Reading": {
                        "insert": [{"label": "Mapped", "properties": {"value": "$.value"}}],
                        "update": [{"label": "Mapped", "properties": {"value": "$.value"}}]
                    }}),
                ),
                config(
                    "relabel",
                    "label",
                    json!({"labelMappings": {"Raw": "Reading"}}),
                ),
            ],
            &["label", "project"],
            "Mapped",
            |value| value,
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builtin_two_producer_graph_current_thread() {
        graph_scenario().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn builtin_two_producer_graph_multi_thread() {
        graph_scenario().await;
    }
}
