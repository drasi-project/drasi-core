// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_bridge_common::{codec::preflight_messagepack, PortableEnvelopeCodec};
use drasi_core::evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue};
use drasi_core_types::data_value::{DataExpression, DataLiteral, DataUnary, DataValue};
use serde::Serialize;

#[tokio::test]
async fn portable_standard_schemas_preserve_exact_binary_context_and_transactions() -> Result<()> {
    for (schema, envelope) in [
        (PortableDelivery::Events, graph_input(u64::MAX, i64::MIN)?),
        (PortableDelivery::Events, query_input(7)?),
        (
            PortableDelivery::CompleteTransactions,
            transaction_input(13)?,
        ),
    ] {
        let codec = PortableEnvelopeCodec::new(schema, namespace(), Limits::default())?;
        let bytes = codec.encode(&envelope)?;
        let decoded = codec.decode(&bytes)?;
        assert_eq!(codec.encode(&decoded)?, bytes);
        assert_eq!(decoded.system(), envelope.system());
        assert_eq!(
            decoded.changes().operations(),
            envelope.changes().operations()
        );
        assert_eq!(
            decoded.annotations().entries().collect::<Vec<_>>(),
            envelope.annotations().entries().collect::<Vec<_>>()
        );
        if schema == PortableDelivery::CompleteTransactions {
            let transaction = SourceTransactionCodec::decode(
                &decoded,
                SourceTransactionLimits {
                    max_changes: NonZeroUsize::new(2).unwrap(),
                    max_bytes: NonZeroUsize::new(65_536).unwrap(),
                    max_duration_ms: std::num::NonZeroU64::new(1_000).unwrap(),
                },
            )?;
            assert_eq!(transaction.position(), b"commit/42");
            assert_eq!(transaction.into_changes().len(), 2);
        }
    }
    Ok(())
}

#[test]
fn namespace_and_wire_versions_do_not_load_or_substitute_a_validator() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    let other_namespace = Namespace {
        version: 1,
        source_prefixes: vec!["elsewhere/".into()],
    };
    let other =
        PortableEnvelopeCodec::new(PortableDelivery::Events, other_namespace, Limits::default())?;
    let bytes = codec.encode(&graph_input(1, 1)?)?;
    assert!(other.decode(&bytes).is_err());
    let transaction = PortableEnvelopeCodec::new(
        PortableDelivery::CompleteTransactions,
        namespace(),
        Limits::default(),
    )?;
    assert!(transaction.decode(&bytes).is_err());
    assert!(
        transaction.encode(&graph_input(1, 1)?).is_err(),
        "an ordinary batch is not a complete transaction merely because it uses the graph schema"
    );
    let mut broken = bytes.clone();
    broken.push(0xc0);
    assert!(codec.decode(&broken).is_err());
    Ok(())
}

#[test]
fn hostile_messagepack_is_rejected_iteratively_before_deserialization() {
    let mut nested = vec![0x91; 20_000];
    nested.push(0);
    assert!(preflight_messagepack(&nested, 100_000, 100_000).is_err());
    assert!(preflight_messagepack(&[0xdd, 0xff, 0xff, 0xff, 0xff], 100, 100).is_err());
    assert!(preflight_messagepack(&[0xdf, 0x80, 0, 0, 0], 100, 100).is_err());
    assert!(preflight_messagepack(&[0xc6, 0xff, 0xff, 0xff, 0xff], 100, 100).is_err());
    assert!(preflight_messagepack(&[0x91, 0], 2, 2).is_ok());
    assert!(preflight_messagepack(&[0x91, 0], 2, 1).is_err());
}

#[derive(Serialize)]
struct Id<'a> {
    namespace: &'a str,
    #[serde(with = "serde_bytes")]
    value: &'a [u8],
}

#[derive(Serialize)]
struct Image<'a> {
    identity: Id<'a>,
    kind: u8,
    #[serde(with = "serde_bytes")]
    bytes: &'a [u8],
}

#[derive(Serialize)]
enum Op<'a> {
    Add { ordinal: u64, after: Image<'a> },
}

#[derive(Serialize)]
struct Descriptor<'a> {
    id: &'a str,
    version: u32,
    encoding: &'a str,
    #[serde(with = "serde_bytes")]
    definition: &'a [u8],
}

#[derive(Clone, Serialize)]
struct Metadata {
    stream: &'static str,
    sequence: u64,
    ordering: &'static str,
    timestamp: Option<String>,
    source_position: Option<Vec<u8>>,
    control: Option<()>,
}

#[derive(Serialize)]
struct RawEnvelope<'a> {
    format: u32,
    id: Id<'a>,
    change_set: Id<'a>,
    schema: Descriptor<'a>,
    operations: Vec<Op<'a>>,
    system: Metadata,
    lineage: Vec<(Id<'a>, Metadata)>,
    context_identity: u64,
    annotations: Vec<()>,
}

fn short_str(out: &mut Vec<u8>, value: &str) {
    assert!(value.len() < 32);
    out.push(0xa0 | value.len() as u8);
    out.extend_from_slice(value.as_bytes());
}

/// Canonical `ElementImage::Node` MessagePack whose only property is `depth`
/// nested lists around a null. It is written without recursion so a hostile
/// depth never builds, or later drops, a typed value.
fn canonical_deep_node(depth: usize) -> Vec<u8> {
    let mut out = vec![0x81];
    short_str(&mut out, "Node");
    out.push(0x82);
    short_str(&mut out, "metadata");
    out.push(0x83);
    short_str(&mut out, "reference");
    out.push(0x82);
    short_str(&mut out, "source_id");
    short_str(&mut out, "plant/db");
    short_str(&mut out, "element_id");
    short_str(&mut out, "deep");
    short_str(&mut out, "labels");
    out.push(0x90);
    short_str(&mut out, "effective_from");
    out.push(1);
    short_str(&mut out, "properties");
    out.push(0x81);
    short_str(&mut out, "deep");
    for _ in 0..depth {
        out.push(0x81);
        short_str(&mut out, "List");
        out.push(0x91);
    }
    short_str(&mut out, "Null");
    out
}

pub(super) fn hostile_envelope(depth: usize, lineage: usize) -> Result<Vec<u8>> {
    let payload = canonical_deep_node(depth);
    let key = GraphChangeCodec::element_reference(&ElementReference::new("plant/db", "deep"))?;
    let reference: &[u8] = key.identity().value();
    let schema = GraphChangeCodec::schema();
    let schema = schema.descriptor();
    let metadata = Metadata {
        stream: "sender/out",
        sequence: 1,
        ordering: "EventTime",
        timestamp: None,
        source_position: None,
        control: None,
    };
    let frame = RawEnvelope {
        format: 1,
        id: Id {
            namespace: "sender/out",
            value: b"id",
        },
        change_set: Id {
            namespace: "sender/out",
            value: b"changes",
        },
        schema: Descriptor {
            id: schema.id().as_str(),
            version: 1,
            encoding: schema.encoding(),
            definition: schema.definition(),
        },
        operations: vec![Op::Add {
            ordinal: 0,
            after: Image {
                identity: Id {
                    namespace: "drasi.graph-element",
                    value: reference,
                },
                kind: 0,
                bytes: &payload,
            },
        }],
        system: metadata.clone(),
        lineage: (0..lineage)
            .map(|_| {
                (
                    Id {
                        namespace: "sender/out",
                        value: b"ancestor",
                    },
                    metadata.clone(),
                )
            })
            .collect(),
        context_identity: 1,
        annotations: vec![],
    };
    Ok(encode_bounded_messagepack(&frame, 1 << 20)?)
}

#[test]
fn opaque_messagepack_depth_and_lineage_are_checked_before_recursive_construction_or_drop(
) -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    assert!(codec.decode(&hostile_envelope(0, 0)?).is_ok());
    assert!(codec.decode(&hostile_envelope(8, 3)?).is_ok());
    assert!(codec.decode(&hostile_envelope(20_000, 0)?).is_err());
    assert!(codec.decode(&hostile_envelope(0, 2_000)?).is_err());
    Ok(())
}

#[test]
fn hostile_fixture_is_the_canonical_node_encoding_and_depth_limits_are_semantic() -> Result<()> {
    let mut value = ElementValue::Null;
    for _ in 0..8 {
        value = ElementValue::List(vec![value]);
    }
    let mut properties = ElementPropertyMap::new();
    properties.insert("deep", value);
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("plant/db", "deep"),
                labels: Arc::from([]),
                effective_from: 1,
            },
            properties,
        },
    };
    let envelope =
        GraphChangeCodec::encode_change(change, StreamId::try_new("sender/out")?, 1, None)?;
    let [ChangeOperation::Added { after, .. }] = envelope.changes().operations() else {
        panic!("expected one insert");
    };
    assert_eq!(&after.payload()[..], canonical_deep_node(8).as_slice());

    // Wrapper variants do not use the value depth budget: the configured number of
    // levels is accepted and one more is rejected, before any typed decoding.
    let limits = Limits::default();
    let codec = PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), limits.clone())?;
    let levels = limits.max_depth;
    assert!(codec.decode(&hostile_envelope(levels - 1, 0)?).is_ok());
    assert!(codec.decode(&hostile_envelope(levels, 0)?).is_err());
    Ok(())
}

#[test]
fn exact_envelope_byte_limit_and_annotation_limits_reject_next_unit() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    let mut envelope = graph_input(1, 1)?;
    envelope.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("test")?,
        "bytes",
        ContextValue::Bytes(Arc::from(vec![1u8; 2048])),
    )?)?;
    let length = codec.encode(&envelope)?.len();
    let exact = PortableEnvelopeCodec::new(
        PortableDelivery::Events,
        namespace(),
        Limits {
            max_envelope_bytes: length,
            ..Limits::default()
        },
    )?;
    let bytes = exact.encode(&envelope)?;
    assert_eq!(exact.encode(&exact.decode(&bytes)?)?.len(), length);
    let short = PortableEnvelopeCodec::new(
        PortableDelivery::Events,
        namespace(),
        Limits {
            max_envelope_bytes: length - 1,
            ..Limits::default()
        },
    )?;
    assert!(short.encode(&envelope).is_err());
    assert!(short.decode(&bytes).is_err());
    let restricted = PortableEnvelopeCodec::new(
        PortableDelivery::Events,
        namespace(),
        Limits {
            max_annotations: 2,
            ..Limits::default()
        },
    )?;
    assert!(restricted.decode(&bytes).is_err());
    Ok(())
}

fn typed(value: drasi_core::evaluation::variable_value::VariableValue) -> Result<ElementValue> {
    let converted: ElementValue = (&value)
        .try_into()
        .map_err(|_| anyhow::anyhow!("value has no element representation"))?;
    Ok(converted)
}

fn metadata_of(id: &str) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("plant/db", id),
        labels: Arc::from([Arc::from("Item")]),
        effective_from: 1_700_000_000_000,
    }
}

fn node(id: &str, properties: ElementPropertyMap) -> Element {
    Element::Node {
        metadata: metadata_of(id),
        properties,
    }
}

fn rich_properties() -> Result<ElementPropertyMap> {
    use chrono::{NaiveDate, NaiveTime};
    use drasi_core::evaluation::variable_value::VariableValue;

    let date = VariableValue::Date(NaiveDate::from_ymd_opt(2026, 10, 12).expect("date"));
    let mut properties = ElementPropertyMap::new();
    properties.insert("date", typed(date.clone())?);
    properties.insert(
        "time",
        typed(VariableValue::LocalTime(
            NaiveTime::from_hms_opt(1, 2, 3).expect("time"),
        ))?,
    );
    properties.insert(
        "reference",
        typed(VariableValue::ElementReference(ElementReference::new(
            "plant/db", "other",
        )))?,
    );
    properties.insert(
        "metadata",
        typed(VariableValue::ElementMetadata(metadata_of("inner")))?,
    );
    properties.insert(
        "nested",
        typed(VariableValue::Element(Arc::new(node(
            "inner",
            ElementPropertyMap::new(),
        ))))?,
    );
    properties.insert(
        "dates",
        ElementValue::List(vec![typed(date)?, ElementValue::Null]),
    );
    properties.insert("plain", ElementValue::Integer(-7));
    Ok(properties)
}

fn change_record(change: SourceChange) -> Result<Record> {
    let envelope =
        GraphChangeCodec::encode_change(change, StreamId::try_new("sender/out")?, 1, None)?;
    Ok(match &envelope.changes().operations()[0] {
        ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
            after.clone()
        }
        ChangeOperation::Deleted { before, .. } => before.clone().expect("metadata before-image"),
    })
}

fn with_payload(record: &Record, image: RecordImage, payload: Vec<u8>) -> Result<Record> {
    Ok(Record::try_new(
        &GraphChangeCodec::schema(),
        record.identity().clone(),
        image,
        Bytes::from(payload),
    )?)
}

fn graph_envelope(
    operations: Vec<ChangeOperation>,
    control: Option<ControlMessage>,
) -> Result<ChangeEnvelope> {
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new("sender/out", Bytes::from_static(b"changes"))?,
        GraphChangeCodec::schema().descriptor().clone(),
        operations,
    )?;
    let mut system = SystemMetadata::new(StreamId::try_new("sender/out")?, 1);
    if let Some(control) = control {
        system = system.with_control(control);
    }
    Ok(ChangeEnvelope::new(
        EnvelopeId::try_new("sender/out", Bytes::from_static(b"id"))?,
        changes,
        system,
    ))
}

fn insert_property(name: &str, value: ElementValue) -> Result<ChangeEnvelope> {
    let mut properties = ElementPropertyMap::new();
    properties.insert(name, value);
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: node("subject", properties),
        },
        StreamId::try_new("sender/out")?,
        1,
        None,
    )?)
}

fn nested_lists(depth: usize) -> ElementValue {
    let mut value = ElementValue::Null;
    for _ in 0..depth {
        value = ElementValue::List(vec![value]);
    }
    value
}

#[test]
fn rich_canonical_patch_partial_and_removal_records_cross_exactly() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    let element = node("rich", rich_properties()?);
    let full = change_record(SourceChange::Insert {
        element: element.clone(),
    })?;
    let patch = change_record(SourceChange::update(element))?;
    let metadata_only = change_record(SourceChange::Delete {
        metadata: metadata_of("rich"),
    })?;
    let relation = change_record(SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata_of("link"),
            in_node: ElementReference::new("plant/db", "a"),
            out_node: ElementReference::new("plant/db", "b"),
            properties: rich_properties()?,
        },
    })?;
    assert_eq!(full.image(), RecordImage::Full);
    assert_eq!(patch.image(), RecordImage::Patch);
    assert_eq!(metadata_only.image(), RecordImage::Partial);

    // An empty canonical removal set is the final array; replace it with two names.
    let mut bytes = patch.payload().to_vec();
    assert_eq!(bytes.pop(), Some(0x90));
    bytes.extend_from_slice(&[0x92, 0xa4]);
    bytes.extend_from_slice(b"gone");
    bytes.extend_from_slice(&[0xa3]);
    bytes.extend_from_slice(b"zed");
    let removal = with_payload(&patch, RecordImage::Patch, bytes)?;
    assert!(GraphChangeCodec::decode_patch(&removal)?
        .removed_properties()
        .contains("gone"));

    // `ElementBeforeImage::Element` wraps one canonical full image.
    let mut bytes = vec![0x81, 0xa7];
    bytes.extend_from_slice(b"Element");
    bytes.extend_from_slice(full.payload());
    let before = with_payload(&full, RecordImage::Partial, bytes)?;
    assert!(matches!(
        GraphChangeCodec::decode_partial(&before)?,
        ElementBeforeImage::Element(_)
    ));

    let envelope = graph_envelope(
        vec![
            ChangeOperation::Added {
                ordinal: 0,
                after: full,
            },
            ChangeOperation::Updated {
                ordinal: 1,
                before: Some(before),
                after: removal,
                semantics: UpdateSemantics::Patch,
            },
            ChangeOperation::Deleted {
                ordinal: 2,
                identity: metadata_only.reference().clone(),
                before: Some(metadata_only),
            },
            ChangeOperation::Added {
                ordinal: 3,
                after: relation,
            },
        ],
        None,
    )?;
    let bytes = codec.encode(&envelope)?;
    let decoded = codec.decode(&bytes)?;
    assert_eq!(
        decoded.changes().operations(),
        envelope.changes().operations()
    );
    assert_eq!(codec.encode(&decoded)?, bytes);
    Ok(())
}

#[test]
fn rich_graph_values_keep_namespaces_and_value_limits() -> Result<()> {
    use drasi_core::evaluation::variable_value::VariableValue;

    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    assert!(codec
        .encode(&insert_property("ok", typed(VariableValue::Null)?)?)
        .is_ok());
    // Ordinary nesting well inside `max_depth` is not an error.
    assert!(codec
        .encode(&insert_property("deep", nested_lists(30))?)
        .is_ok());
    assert!(codec
        .encode(&insert_property("deeper", nested_lists(40))?)
        .is_err());

    let outside = ElementReference::new("elsewhere/db", "x");
    let outside_metadata = ElementMetadata {
        reference: outside.clone(),
        labels: Arc::from([]),
        effective_from: 1,
    };
    for value in [
        VariableValue::ElementReference(outside.clone()),
        VariableValue::ElementMetadata(outside_metadata.clone()),
        VariableValue::Element(Arc::new(Element::Node {
            metadata: outside_metadata.clone(),
            properties: ElementPropertyMap::new(),
        })),
        VariableValue::Element(Arc::new(Element::Relation {
            metadata: metadata_of("inner"),
            in_node: ElementReference::new("plant/db", "a"),
            out_node: outside.clone(),
            properties: ElementPropertyMap::new(),
        })),
    ] {
        let envelope = insert_property("nested", typed(value)?)?;
        assert!(codec.encode(&envelope).is_err());
    }
    let relation = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Relation {
                metadata: metadata_of("link"),
                in_node: ElementReference::new("plant/db", "a"),
                out_node: outside,
                properties: ElementPropertyMap::new(),
            },
        },
        StreamId::try_new("sender/out")?,
        1,
        None,
    )?;
    assert!(codec.encode(&relation).is_err());

    // Awaiting values and expressions are inert shared data on this boundary.
    let envelope = insert_property("awaiting", typed(VariableValue::Awaiting)?)?;
    let bytes = codec.encode(&envelope)?;
    assert_eq!(codec.encode(&codec.decode(&bytes)?)?, bytes);
    Ok(())
}

fn expression(depth: usize) -> DataExpression {
    let mut expression = DataExpression::Unary(DataUnary::Literal(DataLiteral::Integer(1)));
    for _ in 0..depth {
        expression = DataExpression::Binary(
            0,
            Box::new(expression),
            Box::new(DataExpression::Unary(DataUnary::Identifier("x".into()))),
        );
    }
    expression
}

fn data_node(name: &str, value: DataValue) -> Result<ChangeEnvelope> {
    let image = ElementImage::Node {
        metadata: metadata_of("subject"),
        properties: BTreeMap::from([(name.to_string(), value)]),
    };
    graph_envelope(
        vec![ChangeOperation::Added {
            ordinal: 0,
            after: GraphChangeCodec::element_record(&image)?,
        }],
        None,
    )
}

#[test]
fn expressions_and_awaiting_values_are_inert_graph_data_with_limits() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    for value in [
        DataValue::Awaiting,
        DataValue::Expression(expression(0)),
        DataValue::Expression(expression(10)),
        DataValue::List(vec![DataValue::Expression(DataExpression::List(vec![
            expression(2),
            DataExpression::Unary(DataUnary::Literal(DataLiteral::Object(vec![(
                "k".into(),
                DataLiteral::Text("v".into()),
            )]))),
        ]))]),
    ] {
        let bytes = codec.encode(&data_node("value", value)?)?;
        assert_eq!(codec.encode(&codec.decode(&bytes)?)?, bytes);
    }
    // Expressions use the same value depth and size budgets as other values.
    assert!(codec
        .encode(&data_node("deep", DataValue::Expression(expression(40)))?)
        .is_err());
    let limited = PortableEnvelopeCodec::new(
        PortableDelivery::Events,
        namespace(),
        Limits {
            max_values: 8,
            ..Limits::default()
        },
    )?;
    assert!(limited
        .encode(&data_node("wide", DataValue::Expression(expression(10)))?)
        .is_err());
    Ok(())
}

fn future(id: &str) -> drasi_core::interface::FutureElementRef {
    drasi_core::interface::FutureElementRef {
        element_ref: ElementReference::new("plant/db", id),
        original_time: 1,
        due_time: 2,
        group_signature: 3,
    }
}

fn raw_wire(envelope: &ChangeEnvelope) -> Result<Vec<u8>> {
    let mut binary = BinaryEnvelopeCodec::new(NonZeroUsize::new(1 << 20).expect("limit"));
    binary.register_schema(GraphChangeCodec::schema())?;
    Ok(binary.encode(envelope)?)
}

#[test]
fn current_graph_controls_are_rejected_not_silently_ignored() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    let stream = StreamId::try_new("sender/out")?;
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let element = node("subject", ElementPropertyMap::new());
    let controls = [
        GraphChangeCodec::encode_futures_due(stream.clone(), 1, timestamp)?,
        GraphChangeCodec::encode_changes(
            &[SourceChange::Future {
                future_ref: future("due"),
            }],
            stream.clone(),
            1,
            None,
        )?,
        GraphChangeCodec::encode_changes(
            &[
                SourceChange::Insert { element },
                SourceChange::Future {
                    future_ref: future("later"),
                },
            ],
            stream,
            1,
            None,
        )?,
    ];
    for envelope in controls {
        assert!(envelope.system().control().is_some());
        assert!(codec.encode(&envelope).is_err());
        // The same control on an otherwise valid wire frame fails before delivery.
        assert!(codec.decode(&raw_wire(&envelope)?).is_err());
    }
    Ok(())
}

#[test]
fn historical_controls_are_provenance_bounded_only_by_the_frame_budget() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
    let mut source =
        GraphChangeCodec::encode_futures_due(StreamId::try_new("sender/out")?, 1, timestamp)?;
    source.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("clock")?,
        "drasi.query.futures-due",
        ContextValue::Bool(true),
    )?)?;
    let data = graph_input(2, 5)?;
    let derived = source.derive(
        EnvelopeId::try_new("sender/out", Bytes::from_static(b"derived"))?,
        data.changes().clone(),
        SystemMetadata::new(StreamId::try_new("sender/out")?, 2),
    );
    assert!(derived.system().control().is_none());
    let bytes = codec.encode(&derived)?;
    let decoded = codec.decode(&bytes)?;
    assert!(decoded.system().control().is_none());
    assert_eq!(
        decoded.lineage().expect("lineage").system().control(),
        source.system().control()
    );
    assert_eq!(codec.encode(&decoded)?, bytes);
    assert_eq!(decoded.changes().operations(), data.changes().operations());

    // A large historical control is provenance bounded only by the frame budget.
    let large = ControlMessage::try_new("custom/history", Bytes::from(vec![0u8; 70_000]))?;
    let source = ChangeEnvelope::new(
        EnvelopeId::try_new("sender/out", Bytes::from_static(b"big"))?,
        data.changes().clone(),
        SystemMetadata::new(StreamId::try_new("sender/out")?, 1).with_control(large.clone()),
    );
    let derived = source.derive(
        EnvelopeId::try_new("sender/out", Bytes::from_static(b"derived"))?,
        data.changes().clone(),
        SystemMetadata::new(StreamId::try_new("sender/out")?, 2),
    );
    let bytes = codec.encode(&derived)?;
    assert_eq!(
        codec
            .decode(&bytes)?
            .lineage()
            .expect("lineage")
            .system()
            .control(),
        Some(&large)
    );
    let small = PortableEnvelopeCodec::new(
        PortableDelivery::Events,
        namespace(),
        Limits {
            max_envelope_bytes: 4096,
            ..Limits::default()
        },
    )?;
    assert!(small.encode(&derived).is_err());
    assert!(small.decode(&bytes).is_err());
    Ok(())
}

#[test]
fn rich_source_transaction_values_keep_validation_and_namespaces() -> Result<()> {
    use drasi_core::evaluation::variable_value::VariableValue;

    let codec = PortableEnvelopeCodec::new(
        PortableDelivery::CompleteTransactions,
        namespace(),
        Limits::default(),
    )?;
    let transaction = |properties: ElementPropertyMap| -> Result<ChangeEnvelope> {
        let limits = SourceTransactionLimits {
            max_changes: NonZeroUsize::new(10).expect("nonzero change limit"),
            max_bytes: NonZeroUsize::new(65_536).expect("nonzero byte limit"),
            max_duration_ms: std::num::NonZeroU64::new(10_000).expect("nonzero duration limit"),
        };
        let mut builder = SourceTransactionBuilder::new("plant/db", limits)?;
        builder.push(SourceChange::Insert {
            element: node("subject", properties),
        })?;
        Ok(builder
            .commit(
                Bytes::from_static(b"transaction-1"),
                Bytes::from_static(b"commit/42"),
            )?
            .into_envelope(
                StreamId::try_new("sender/out")?,
                1,
                chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
            )?)
    };
    let envelope = transaction(rich_properties()?)?;
    let bytes = codec.encode(&envelope)?;
    assert_eq!(codec.encode(&codec.decode(&bytes)?)?, bytes);

    let mut properties = ElementPropertyMap::new();
    properties.insert(
        "reference",
        typed(VariableValue::ElementReference(ElementReference::new(
            "elsewhere/db",
            "x",
        )))?,
    );
    assert!(codec.encode(&transaction(properties)?).is_err());
    Ok(())
}

#[test]
fn source_transaction_updates_with_removals_and_replacements_cross_the_bridge() -> Result<()> {
    let codec = PortableEnvelopeCodec::new(
        PortableDelivery::CompleteTransactions,
        namespace(),
        Limits::default(),
    )?;
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(10).expect("nonzero change limit"),
        max_bytes: NonZeroUsize::new(65_536).expect("nonzero byte limit"),
        max_duration_ms: std::num::NonZeroU64::new(10_000).expect("nonzero duration limit"),
    };
    let build = |removed: &[&str]| -> Result<ChangeEnvelope> {
        let mut builder = SourceTransactionBuilder::new("plant/db", limits)?;
        builder.push(SourceChange::Insert {
            element: node("subject", ElementPropertyMap::new()),
        })?;
        builder.push(SourceChange::update_with_removals(
            node("subject", ElementPropertyMap::new()),
            removed.iter().map(|name| name.to_string()).collect(),
        )?)?;
        builder.push(SourceChange::replace(node(
            "subject",
            ElementPropertyMap::new(),
        )))?;
        Ok(builder
            .commit(
                Bytes::from_static(b"transaction-1"),
                Bytes::from_static(b"commit/42"),
            )?
            .into_envelope(
                StreamId::try_new("sender/out")?,
                1,
                chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
            )?)
    };
    let envelope = build(&["gone", "also-gone"])?;
    let bytes = codec.encode(&envelope)?;
    let decoded = codec.decode(&bytes)?;
    assert_eq!(codec.encode(&decoded)?, bytes);
    let changes = SourceTransactionCodec::decode(&decoded, limits)?.into_changes();
    assert!(matches!(
        changes.as_slice(),
        [
            SourceChange::Insert { .. },
            SourceChange::Update { removed_properties, .. },
            SourceChange::Replace { .. },
        ] if removed_properties.len() == 2
    ));

    // A removal name beyond the string limit is rejected before decoding it.
    let oversized = "x".repeat(257);
    assert!(codec.encode(&build(&[oversized.as_str()])?).is_err());
    Ok(())
}

#[test]
fn complete_transaction_markers_are_scoped_to_their_own_change_set() -> Result<()> {
    let atomic = PortableEnvelopeCodec::new(
        PortableDelivery::CompleteTransactions,
        namespace(),
        Limits::default(),
    )?;
    let events =
        PortableEnvelopeCodec::new(PortableDelivery::Events, namespace(), Limits::default())?;
    let transaction = transaction_input(5)?;

    // The identical group crosses either delivery mode with its marker intact,
    // and only atomic delivery requires it.
    for codec in [&atomic, &events] {
        let bytes = codec.encode(&transaction)?;
        let decoded = codec.decode(&bytes)?;
        assert!(SourceTransactionCodec::is_complete_transaction(&decoded));
        assert_eq!(codec.encode(&decoded)?, bytes);
    }

    // Transformed output inherits annotations but not completeness.
    let transformed = GraphChangeCodec::derive_changes(
        &transaction,
        &GraphChangeCodec::decode_changes(&transaction)?,
        StreamId::try_new("sender/out")?,
        6,
    )?;
    assert!(!SourceTransactionCodec::is_complete_transaction(
        &transformed
    ));
    assert!(atomic.encode(&transformed).is_err());
    assert!(events.encode(&transformed).is_ok());

    // A malformed or contradictory current marker fails even for event delivery.
    let ordinary = graph_input(5, 1)?;
    let mut forged = ordinary.clone();
    forged.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("plant/db")?,
        "drasi.source-transaction.v1",
        ContextValue::Bytes(Arc::from(serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "change_set_namespace": ordinary.changes().id().namespace(),
            "change_set_value": ordinary
                .changes()
                .id()
                .value()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "source_id": "plant/db",
            "transaction_id": "00",
            "position": "00",
            "operations": 99,
            "complete": true,
        }))?)),
    )?)?;
    assert!(events.encode(&forged).is_err());
    assert!(atomic.encode(&forged).is_err());
    Ok(())
}

fn graph_codec(namespace: Namespace) -> Result<PortableEnvelopeCodec> {
    Ok(PortableEnvelopeCodec::new(
        PortableDelivery::Events,
        namespace,
        Limits::default(),
    )?)
}

fn only(entries: &[&str]) -> Namespace {
    Namespace {
        version: 1,
        source_prefixes: entries.iter().map(|entry| entry.to_string()).collect(),
    }
}

#[test]
fn source_and_query_records_traverse_the_same_bridge_schema() -> Result<()> {
    assert_eq!(
        QueryChangeCodec::schema().descriptor(),
        GraphChangeCodec::schema().descriptor()
    );
    assert_eq!(
        PortableDelivery::Events.schema().descriptor(),
        GraphChangeCodec::schema().descriptor()
    );
    let codec = graph_codec(namespace())?;
    let source = graph_input(1, 1)?;
    let query = query_input(2)?;
    assert!(!QueryChangeCodec::is_query_output(&source));
    assert!(QueryChangeCodec::is_query_output(&query));
    for envelope in [&source, &query] {
        assert_eq!(
            envelope.changes().schema(),
            PortableDelivery::Events.schema().descriptor()
        );
        // One codec carries both: there is no role-specific record encoding.
        let bytes = codec.encode(envelope)?;
        let decoded = codec.decode(&bytes)?;
        assert_eq!(
            decoded.changes().operations(),
            envelope.changes().operations()
        );
        assert_eq!(codec.encode(&decoded)?, bytes);
    }

    // A query row is an ordinary node of its derived dataset namespace.
    let [ChangeOperation::Added { after, .. }] = query.changes().operations() else {
        panic!("expected one added result node");
    };
    assert_eq!(after.image(), RecordImage::Full);
    let image = ElementImage::decode(after.payload(), after.payload().len())?;
    assert_eq!(
        &*image.metadata().reference.source_id,
        query_dataset().as_str()
    );
    assert!(query_dataset().starts_with("drasi.output/"));
    Ok(())
}

#[test]
fn query_metadata_is_not_required_to_transport_canonical_nodes() -> Result<()> {
    let codec = graph_codec(namespace())?;
    let query = query_input(3)?;
    // The same change set and records without any query annotation.
    let bare = ChangeEnvelope::new(
        query.id().clone(),
        query.changes().clone(),
        query.system().as_ref().clone(),
    );
    assert!(!QueryChangeCodec::is_query_output(&bare));
    assert_eq!(bare.annotations().len(), 0);
    let bytes = codec.encode(&bare)?;
    let decoded = codec.decode(&bytes)?;
    assert_eq!(decoded.changes().operations(), query.changes().operations());

    // Hand-built result nodes of the dataset need no projection context either.
    let projector = query_projector();
    let values = BTreeMap::from([("n".into(), VariableValue::Integer(1.into()))]);
    let record = projector.record(&ResultNodeProjector::signature_identity(9), &values, 1)?;
    let envelope = graph_envelope(
        vec![ChangeOperation::Added {
            ordinal: 0,
            after: record,
        }],
        None,
    )?;
    let bytes = codec.encode(&envelope)?;
    assert_eq!(codec.encode(&codec.decode(&bytes)?)?, bytes);
    Ok(())
}

#[test]
fn query_updates_removals_and_patches_use_the_canonical_record_path() -> Result<()> {
    let codec = graph_codec(namespace())?;
    let projector = query_projector();
    let row = |value: i64| BTreeMap::from([("n".into(), VariableValue::Integer(value.into()))]);
    let results = [
        QueryPartEvaluationContext::Updating {
            before: row(1),
            after: row(2),
            row_signature: 1,
        },
        QueryPartEvaluationContext::Removing {
            before: row(3),
            row_signature: 2,
        },
        QueryPartEvaluationContext::Aggregation {
            before: None,
            after: row(4),
            grouping_keys: vec!["n".into()],
            default_before: false,
            default_after: true,
            row_signature: 3,
        },
    ];
    let envelope = query_results(4, &projector, &results)?;
    assert_eq!(envelope.changes().operations().len(), 3);
    let bytes = codec.encode(&envelope)?;
    let decoded = codec.decode(&bytes)?;
    assert_eq!(
        decoded.changes().operations(),
        envelope.changes().operations()
    );
    assert_eq!(codec.encode(&decoded)?, bytes);

    // A canonical patch with removed properties and a metadata before-image in
    // the same derived namespace crosses exactly like a source patch.
    let dataset = query_dataset();
    let metadata = |id: &str| ElementMetadata {
        reference: ElementReference::new(dataset.as_str(), id),
        labels: Arc::from([Arc::from("Result")]),
        effective_from: 1,
    };
    let mut properties = ElementPropertyMap::new();
    properties.insert("n", ElementValue::Integer(5));
    let patch = change_record(SourceChange::Update {
        removed_properties: Default::default(),
        element: Element::Node {
            metadata: metadata("aa"),
            properties,
        },
    })?;
    assert_eq!(patch.image(), RecordImage::Patch);
    let mut payload = patch.payload().to_vec();
    assert_eq!(payload.pop(), Some(0x90));
    payload.extend_from_slice(&[0x91, 0xa4]);
    payload.extend_from_slice(b"gone");
    let removal = with_payload(&patch, RecordImage::Patch, payload)?;
    let full = change_record(SourceChange::Insert {
        element: Element::Node {
            metadata: metadata("aa"),
            properties: ElementPropertyMap::new(),
        },
    })?;
    let mut bytes = vec![0x81, 0xa7];
    bytes.extend_from_slice(b"Element");
    bytes.extend_from_slice(full.payload());
    let before = with_payload(&full, RecordImage::Partial, bytes)?;
    let gone = change_record(SourceChange::Delete {
        metadata: metadata("bb"),
    })?;
    let envelope = graph_envelope(
        vec![
            ChangeOperation::Updated {
                ordinal: 0,
                before: Some(before),
                after: removal,
                semantics: UpdateSemantics::Patch,
            },
            ChangeOperation::Deleted {
                ordinal: 1,
                identity: gone.reference().clone(),
                before: Some(gone),
            },
        ],
        None,
    )?;
    let bytes = codec.encode(&envelope)?;
    assert_eq!(
        codec.decode(&bytes)?.changes().operations(),
        envelope.changes().operations()
    );
    Ok(())
}

#[test]
fn query_output_namespaces_are_exact_and_embedded_references_are_still_checked() -> Result<()> {
    let dataset = query_dataset();
    let query = query_input(5)?;

    // Physical source authority does not authorize a derived output.
    assert!(graph_codec(only(&["plant/"]))?.encode(&query).is_err());
    let allowed = graph_codec(only(&["plant/", dataset.as_str()]))?;
    let bytes = allowed.encode(&query)?;
    assert!(graph_codec(only(&["plant/"]))?.decode(&bytes).is_err());
    // The reverse is also precise: the dataset alone does not authorize sources.
    assert!(graph_codec(only(&[dataset.as_str()]))?
        .encode(&graph_input(1, 1)?)
        .is_err());

    // Another output's exact dataset, or one extending this dataset, is outside.
    let other = drasi_computation_contracts::output_identity::dataset_namespace(
        "left",
        GRAPH,
        &ComponentId::try_new("plant/other")?,
        &PortId::try_new("out")?,
    )?;
    assert_ne!(other, dataset);
    assert!(graph_codec(only(&[other.as_str()]))?
        .encode(&query)
        .is_err());
    let extended = ResultNodeProjector::try_new(&format!("{dataset}x"), None)?;
    let sibling = query_results(
        6,
        &extended,
        &[QueryPartEvaluationContext::Adding {
            after: BTreeMap::new(),
            row_signature: 1,
        }],
    )?;
    assert!(allowed.encode(&sibling).is_err());
    // A prefix entry is explicit: it cannot be the bare namespace.
    assert!(graph_codec(only(&["drasi.output/"]))?
        .encode(&query)
        .is_ok());
    assert!(graph_codec(only(&["drasi.output/"]))?
        .encode(&query_results(
            7,
            &ResultNodeProjector::try_new("drasi.output/", None)?,
            &[QueryPartEvaluationContext::Adding {
                after: BTreeMap::new(),
                row_signature: 1,
            }],
        )?)
        .is_err());

    // Embedded references in query values are checked recursively.
    let projector = query_projector();
    let embedded = |source: &str| -> Result<ChangeEnvelope> {
        let values = BTreeMap::from([(
            "ref".into(),
            VariableValue::ElementReference(ElementReference::new(source, "x")),
        )]);
        graph_envelope(
            vec![ChangeOperation::Added {
                ordinal: 0,
                after: projector.record(&ResultNodeProjector::signature_identity(1), &values, 1)?,
            }],
            None,
        )
    };
    assert!(allowed.encode(&embedded("plant/db")?).is_ok());
    assert!(allowed.encode(&embedded("elsewhere/db")?).is_err());
    let bytes = graph_codec(only(&["plant/", "elsewhere/", dataset.as_str()]))?
        .encode(&embedded("elsewhere/db")?)?;
    assert!(allowed.decode(&bytes).is_err());

    // A prefix needs at least one more character.
    let bare_prefix = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("plant/", "x"),
                    labels: Arc::from([]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::new(),
            },
        },
        StreamId::try_new("sender/out")?,
        1,
        None,
    )?;
    assert!(allowed.encode(&bare_prefix).is_err());
    Ok(())
}

#[test]
fn query_snapshot_and_progress_frames_are_recovery_controls_not_bridge_data() -> Result<()> {
    let codec = graph_codec(namespace())?;
    let projector = query_projector();
    let component = ComponentId::try_new("plant/query")?;
    let system = |sequence| -> Result<SystemMetadata> {
        Ok(SystemMetadata::new(
            StreamId::try_new("sender/out")?,
            sequence,
        ))
    };
    let values = BTreeMap::from([("n".into(), VariableValue::Integer(1.into()))]);
    let row = projector.record(&ResultNodeProjector::signature_identity(7), &values, 1)?;
    let snapshot = QueryResultSnapshot {
        projector: projector.clone(),
        as_of_sequence: 3,
        generation: 1,
        rows: std::iter::once((row.identity().clone(), row)).collect(),
    };
    let snapshot = QueryChangeCodec::snapshot_envelope(
        &projector,
        "plant/query",
        &snapshot,
        &component,
        system(4)?,
    )?;
    let progress = QueryChangeCodec::progress_envelope(
        &projector,
        "plant/query",
        5,
        1,
        &component,
        system(5)?,
    )?;
    assert!(QueryChangeCodec::is_snapshot(&snapshot));
    assert!(QueryChangeCodec::is_progress_only(&progress));
    for control in [&snapshot, &progress] {
        let error = codec.encode(control).unwrap_err();
        assert!(error.to_string().contains("recovery controls"), "{error}");
        assert!(codec.decode(&raw_wire(control)?).is_err());
    }

    // The same rows as live output are ordinary data.
    let live = query_input(6)?;
    assert!(!QueryChangeCodec::is_snapshot(&live));
    assert!(codec.encode(&live).is_ok());

    // Annotations inherited from an earlier recovery frame are provenance, not
    // a command for the new envelope.
    let derived = snapshot.derive(
        EnvelopeId::try_new("sender/out", Bytes::from_static(b"derived"))?,
        graph_input(7, 1)?.changes().clone(),
        system(7)?,
    );
    assert!(!QueryChangeCodec::is_snapshot(&derived));
    let bytes = codec.encode(&derived)?;
    assert_eq!(
        codec.decode(&bytes)?.changes().operations(),
        derived.changes().operations()
    );
    Ok(())
}

#[test]
fn retired_query_change_selector_and_query_prefixes_reject() -> Result<()> {
    // Neither a query selector nor a transaction-specific data schema selector exists.
    for retired in [
        "query-change",
        "QueryChange",
        "graph-change",
        "source-transaction",
    ] {
        assert!(serde_json::from_str::<PortableDelivery>(&format!("\"{retired}\"")).is_err());
    }
    for delivery in ["events", "complete-transactions"] {
        assert!(serde_json::from_str::<PortableDelivery>(&format!("\"{delivery}\"")).is_ok());
    }
    assert!(serde_json::from_value::<Namespace>(serde_json::json!({
        "version": 1,
        "source_prefixes": ["plant/"],
        "query_prefixes": ["plant/"],
    }))
    .is_err());

    let directory = Directory::new()?;
    let config = configuration(
        &directory,
        true,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    let mut value = serde_json::to_value(&config)?;
    assert_eq!(
        serde_json::from_value::<BridgeConfig>(value.clone())?,
        config
    );
    let mut retired = value.clone();
    retired["schema"] = serde_json::json!("graph-change");
    assert!(serde_json::from_value::<BridgeConfig>(retired).is_err());
    let mut selector = value.clone();
    selector["delivery"] = serde_json::json!("graph-change");
    assert!(serde_json::from_value::<BridgeConfig>(selector).is_err());
    value["namespace"]["query_prefixes"] = serde_json::json!(["plant/"]);
    assert!(serde_json::from_value::<BridgeConfig>(value).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn namespace_entries_are_prefixes_or_exact_names_and_never_empty() -> Result<()> {
    let directory = Directory::new()?;
    let mut config = configuration(
        &directory,
        true,
        Profile::Volatile,
        PortableDelivery::Events,
    );
    assert!(config.validate().is_ok());
    config.namespace = only(&[query_dataset().as_str()]);
    assert!(config.validate().is_ok());
    config.namespace = only(&[]);
    assert!(config.validate().is_err());
    config.namespace = only(&[""]);
    assert!(config.validate().is_err());
    config.namespace = Namespace {
        version: 1,
        source_prefixes: vec!["plant/".into(); 17],
    };
    assert!(config.validate().is_err());
    config.namespace = Namespace {
        version: 2,
        source_prefixes: vec!["plant/".into()],
    };
    assert!(config.validate().is_err());
    Ok(())
}
