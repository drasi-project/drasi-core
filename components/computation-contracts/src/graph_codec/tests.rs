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
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
};

use chrono::{DateTime, NaiveDate};
use drasi_core_types::{
    data_value::DataValue,
    models::{ElementPropertyMap, ElementValue},
};

use super::*;
use crate::{BinaryEnvelopeCodec, ChangeOrdering, EnvelopeCodec};

const LIMIT: usize = 1 << 20;

#[test]
fn graph_element_keys_reject_alternate_serializations_of_the_same_reference() {
    let reference = ElementReference::new("s", "n");
    let positional = rmp_serde::to_vec(&reference).unwrap();
    assert!(RecordReference::try_new(
        &GraphChangeCodec::schema(),
        RecordId::try_new("drasi.graph-element", Bytes::from(positional)).unwrap(),
    )
    .is_err());
    assert!(GraphChangeCodec::element_reference(&reference).is_ok());
}

/// Canonical named MessagePack of
/// `ElementImage::Node { metadata: (s/n, ["L"], 5), properties: {"a": Bool(true)} }`.
#[rustfmt::skip]
const GOLDEN_NODE_IMAGE: &[u8] = &[
    0x81, 0xa4, b'N', b'o', b'd', b'e',                                 // {"Node":
    0x82,                                                               //   {
    0xa8, b'm', b'e', b't', b'a', b'd', b'a', b't', b'a',               //     "metadata":
    0x83,                                                               //       {
    0xa9, b'r', b'e', b'f', b'e', b'r', b'e', b'n', b'c', b'e',         //         "reference":
    0x82,                                                               //           {
    0xa9, b's', b'o', b'u', b'r', b'c', b'e', b'_', b'i', b'd',         //             "source_id":
    0xa1, b's',                                                         //               "s",
    0xaa, b'e', b'l', b'e', b'm', b'e', b'n', b't', b'_', b'i', b'd',   //             "element_id":
    0xa1, b'n',                                                         //               "n"},
    0xa6, b'l', b'a', b'b', b'e', b'l', b's',                           //         "labels":
    0x91, 0xa1, b'L',                                                   //           ["L"],
    0xae, b'e', b'f', b'f', b'e', b'c', b't', b'i', b'v', b'e', b'_',
    b'f', b'r', b'o', b'm',                                             //         "effective_from":
    0x05,                                                               //           5},
    0xaa, b'p', b'r', b'o', b'p', b'e', b'r', b't', b'i', b'e', b's',   //     "properties":
    0x81, 0xa1, b'a',                                                   //       {"a":
    0x81, 0xa4, b'B', b'o', b'o', b'l', 0xc3,                           //         {"Bool": true}}}}
];

/// Canonical named MessagePack of `ElementReference { source_id: "s", element_id: "n" }`.
#[rustfmt::skip]
const GOLDEN_REFERENCE: &[u8] = &[
    0x82,
    0xa9, b's', b'o', b'u', b'r', b'c', b'e', b'_', b'i', b'd',
    0xa1, b's',
    0xaa, b'e', b'l', b'e', b'm', b'e', b'n', b't', b'_', b'i', b'd',
    0xa1, b'n',
];

fn reference(id: &str) -> ElementReference {
    ElementReference::new("source", id)
}

fn metadata(id: &str, labels: &[&str], effective_from: u64) -> ElementMetadata {
    ElementMetadata {
        reference: reference(id),
        labels: labels.iter().map(|label| Arc::from(*label)).collect(),
        effective_from,
    }
}

fn data(entries: Vec<(&str, DataValue)>) -> BTreeMap<String, DataValue> {
    entries
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

fn node_image(id: &str, at: u64, entries: Vec<(&str, DataValue)>) -> ElementImage {
    ElementImage::Node {
        metadata: metadata(id, &["Node"], at),
        properties: data(entries),
    }
}

fn relation_image(id: &str, at: u64, entries: Vec<(&str, DataValue)>) -> ElementImage {
    ElementImage::Relation {
        metadata: metadata(id, &["KNOWS"], at),
        in_node: ElementReference::new("origin", "start"),
        out_node: ElementReference::new("target", "end"),
        properties: data(entries),
    }
}

fn text(value: &str) -> ElementValue {
    ElementValue::String(Arc::from(value))
}

fn node(id: &str, at: u64, entries: Vec<(&str, ElementValue)>) -> Element {
    let mut properties = ElementPropertyMap::new();
    for (name, value) in entries {
        properties.insert(name, value);
    }
    Element::Node {
        metadata: metadata(id, &["Node"], at),
        properties,
    }
}

fn property_names(element: &Element) -> Vec<String> {
    let mut names: Vec<String> = element
        .get_properties()
        .map_iter(|name, _| name.to_string())
        .collect();
    names.sort();
    names
}

fn at(millis: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(millis).unwrap()
}

fn stream(name: &str) -> StreamId {
    StreamId::try_new(name).unwrap()
}

fn future(id: &str, due_time: u64) -> FutureElementRef {
    FutureElementRef {
        element_ref: reference(id),
        original_time: 1,
        due_time,
        group_signature: 99,
    }
}

fn rich_properties() -> Vec<(&'static str, DataValue)> {
    let mut nested = BTreeMap::new();
    nested.insert("list".to_string(), DataValue::List(vec![DataValue::Null]));
    nested.insert(
        "reference".to_string(),
        DataValue::Reference(ElementReference::new("other", "r")),
    );
    vec![
        ("", DataValue::String("empty name".to_string())),
        ("null", DataValue::Null),
        ("bool", DataValue::Bool(false)),
        ("min", DataValue::Signed(i64::MIN)),
        ("max", DataValue::Unsigned(u64::MAX)),
        ("nan", DataValue::Float(0x7ff8_0000_0000_0042)),
        ("neg_zero", DataValue::Float((-0.0_f64).to_bits())),
        (
            "date",
            DataValue::Date(NaiveDate::from_ymd_opt(2026, 2, 28).unwrap()),
        ),
        (
            "zoned",
            DataValue::ZonedDateTime(
                DateTime::parse_from_rfc3339("2026-03-29T01:30:00.5+01:00").unwrap(),
                Some("Europe/Paris".to_string()),
            ),
        ),
        (
            "duration",
            DataValue::Duration {
                seconds: -5,
                nanos: -500,
                years: 2,
                months: -3,
            },
        ),
        ("range", DataValue::Range(None, Some(-1))),
        ("metadata", DataValue::Metadata(metadata("m", &[], 0))),
        ("object", DataValue::Object(nested)),
        (
            "element",
            DataValue::Element(node("embedded", 3, vec![("k", text("v"))])),
        ),
        ("awaiting", DataValue::Awaiting),
    ]
}

fn identity(id: &str) -> RecordId {
    GraphChangeCodec::element_reference(&reference(id))
        .unwrap()
        .identity()
        .clone()
}

fn added(ordinal: u64, image: &ElementImage) -> ChangeOperation {
    ChangeOperation::Added {
        ordinal,
        after: GraphChangeCodec::element_record(image).unwrap(),
    }
}

fn deleted(ordinal: u64, id: &str, before: Option<Record>) -> ChangeOperation {
    ChangeOperation::Deleted {
        ordinal,
        identity: GraphChangeCodec::element_reference(&reference(id)).unwrap(),
        before,
    }
}

fn envelope(operations: Vec<ChangeOperation>, timestamp: Option<DateTime<Utc>>) -> ChangeEnvelope {
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new("test", Bytes::from_static(&[1])).unwrap(),
        GraphChangeCodec::schema().descriptor().clone(),
        operations,
    )
    .unwrap();
    let mut system = SystemMetadata::new(stream("test"), 1);
    if let Some(timestamp) = timestamp {
        system = system.with_timestamp(timestamp);
    }
    ChangeEnvelope::new(
        crate::emission_id(&stream("test"), 1).unwrap(),
        changes,
        system,
    )
}

fn envelope_of_update(after: Record) -> ChangeEnvelope {
    envelope(
        vec![ChangeOperation::Updated {
            ordinal: 0,
            before: None,
            after,
            semantics: UpdateSemantics::Patch,
        }],
        None,
    )
}

fn with_control(source: &ChangeEnvelope, control: Option<ControlMessage>) -> ChangeEnvelope {
    let mut system =
        SystemMetadata::new(source.system().stream().clone(), source.system().sequence());
    if let Some(timestamp) = source.system().timestamp() {
        system = system.with_timestamp(timestamp);
    }
    if let Some(control) = control {
        system = system.with_control(control);
    }
    ChangeEnvelope::new(source.id().clone(), source.changes().clone(), system)
}

fn reevaluate(entries: Vec<(u64, FutureElementRef)>) -> ControlMessage {
    let futures = entries
        .into_iter()
        .map(|(ordinal, future_ref)| OrderedFuture {
            ordinal,
            future_ref,
        })
        .collect();
    ControlMessage::try_new(
        GRAPH_CONTROL,
        encode(&GraphControl::Reevaluate(futures)).unwrap(),
    )
    .unwrap()
}

fn control_with_payload(kind: &str, payload: Vec<u8>) -> ControlMessage {
    ControlMessage::try_new(kind, Bytes::from(payload)).unwrap()
}

fn reevaluations(envelope: &ChangeEnvelope) -> Vec<(u64, FutureElementRef)> {
    match GraphChangeCodec::control(envelope).unwrap() {
        Some(GraphControl::Reevaluate(futures)) => futures
            .into_iter()
            .map(|future| (future.ordinal, future.future_ref))
            .collect(),
        other => panic!("expected a reevaluation control, found {other:?}"),
    }
}

fn binary_codec() -> BinaryEnvelopeCodec {
    let mut codec = BinaryEnvelopeCodec::new(NonZeroUsize::new(LIMIT).unwrap());
    codec.register_schema(GraphChangeCodec::schema()).unwrap();
    codec
}

fn json_codec() -> EnvelopeCodec {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(LIMIT).unwrap());
    codec.register_schema(GraphChangeCodec::schema()).unwrap();
    codec
}

/// The envelope as decoded by the binary and by the JSON codec, in that order.
fn through_codecs(envelope: &ChangeEnvelope) -> [ChangeEnvelope; 2] {
    let binary = binary_codec();
    let json = json_codec();
    [
        binary.decode(&binary.encode(envelope).unwrap()).unwrap(),
        json.decode(&json.encode(envelope).unwrap()).unwrap(),
    ]
}

fn replace_once(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let position = bytes
        .windows(from.len())
        .position(|window| window == from)
        .expect("marker is present");
    let mut replaced = bytes.to_vec();
    replaced[position..position + from.len()].copy_from_slice(to);
    replaced
}

fn assert_control_error(envelope: &ChangeEnvelope) {
    assert!(
        matches!(
            GraphChangeCodec::decode_changes(envelope),
            Err(GraphCodecError::Control(_))
        ),
        "expected a control error"
    );
}

fn decode_record(record: &Record) -> Result<(), GraphCodecError> {
    match record.image() {
        RecordImage::Full => GraphChangeCodec::decode_element(record).map(drop),
        RecordImage::Patch => GraphChangeCodec::decode_patch(record).map(drop),
        RecordImage::Partial => GraphChangeCodec::decode_partial(record).map(drop),
    }
}

/// Accepts every record, so tests can build records the canonical validator refuses.
struct AcceptAll;

impl RecordValidator for AcceptAll {
    fn validate_identity(
        &self,
        _: &SchemaDescriptor,
        _: &RecordId,
    ) -> Result<(), RecordValidationError> {
        Ok(())
    }

    fn validate(
        &self,
        _: &SchemaDescriptor,
        _: &RecordId,
        _: RecordImage,
        _: &[u8],
    ) -> Result<(), RecordValidationError> {
        Ok(())
    }
}

fn lenient_graph_schema() -> Schema {
    Schema::new(
        GraphChangeCodec::schema().descriptor().clone(),
        Arc::new(AcceptAll),
    )
}

fn foreign_descriptor(id: &str, version: u32) -> SchemaDescriptor {
    SchemaDescriptor::try_new(
        SchemaId::try_new(id).unwrap(),
        SchemaVersion::try_new(version).unwrap(),
        "drasi-element-msgpack-v1",
        Bytes::from_static(b"foreign"),
    )
    .unwrap()
}

fn mixed_changes() -> Vec<SourceChange> {
    vec![
        SourceChange::Insert {
            element: node("a", 1, vec![("name", text("a"))]),
        },
        SourceChange::Future {
            future_ref: future("a", 100),
        },
        SourceChange::Update {
            removed_properties: Default::default(),
            element: node("b", 2, vec![("flag", ElementValue::Bool(true))]),
        },
        SourceChange::Future {
            future_ref: future("b", 200),
        },
        SourceChange::Delete {
            metadata: metadata("c", &["Gone"], 42),
        },
    ]
}

#[test]
fn golden_node_full_image_uses_named_messagepack_without_a_nested_operation() {
    let short = |id: &str, labels: &[&str], effective_from: u64| ElementMetadata {
        reference: ElementReference::new("s", id),
        labels: labels.iter().map(|label| Arc::from(*label)).collect(),
        effective_from,
    };
    let image = ElementImage::Node {
        metadata: short("n", &["L"], 5),
        properties: data(vec![("a", DataValue::Bool(true))]),
    };
    assert_eq!(image.encode(LIMIT).unwrap(), GOLDEN_NODE_IMAGE);

    let record = GraphChangeCodec::element_record(&image).unwrap();
    assert_eq!(record.image(), RecordImage::Full);
    assert_eq!(record.payload().as_ref(), GOLDEN_NODE_IMAGE);
    assert_eq!(record.identity().namespace(), GRAPH_IDENTITY);
    assert_eq!(record.identity().value().as_ref(), GOLDEN_REFERENCE);
    assert_eq!(GraphChangeCodec::decode_element(&record).unwrap(), image);
    assert_eq!(
        ElementImage::decode(GOLDEN_NODE_IMAGE, GOLDEN_NODE_IMAGE.len()).unwrap(),
        image
    );

    // The same element converts to the same canonical image for every producer.
    let element = Element::Node {
        metadata: short("n", &["L"], 5),
        properties: {
            let mut properties = ElementPropertyMap::new();
            properties.insert("a", ElementValue::Bool(true));
            properties
        },
    };
    assert_eq!(ElementImage::try_from(&element).unwrap(), image);

    for operation in ["Insert", "Update", "Delete", "Future"] {
        assert!(!GOLDEN_NODE_IMAGE
            .windows(operation.len())
            .any(|window| window == operation.as_bytes()));
    }
}

#[test]
fn schema_descriptor_is_exactly_graph_change_v1() {
    let schema = GraphChangeCodec::schema();
    let descriptor = schema.descriptor();
    assert_eq!(descriptor.id().as_str(), "drasi.graph-change");
    assert_eq!(descriptor.version().value(), 1);
    assert_eq!(descriptor.encoding(), "drasi-element-msgpack-v1");
    assert_eq!(BinaryEnvelopeCodec::VERSION, 1);
}

#[test]
fn full_images_roundtrip_nodes_and_relations_with_rich_values() {
    for image in [
        node_image("node", u64::MAX, rich_properties()),
        relation_image("rel", 17, rich_properties()),
    ] {
        let record = GraphChangeCodec::element_record(&image).unwrap();
        assert_eq!(record.image(), RecordImage::Full);
        assert_eq!(
            record.reference(),
            &GraphChangeCodec::element_reference(&image.metadata().reference).unwrap()
        );
        let decoded = GraphChangeCodec::decode_element(&record).unwrap();
        assert_eq!(decoded, image);
        assert_eq!(decoded.properties()["max"], DataValue::Unsigned(u64::MAX));
        assert_eq!(decoded.properties()["min"], DataValue::Signed(i64::MIN));
        assert_eq!(
            decoded.properties()["nan"],
            DataValue::Float(0x7ff8_0000_0000_0042)
        );
        assert_eq!(
            GraphChangeCodec::element_record(&decoded)
                .unwrap()
                .payload(),
            record.payload()
        );
    }
}

#[test]
fn each_decoder_accepts_only_its_own_image_kind() {
    let image = node_image("n", 1, vec![]);
    let full = GraphChangeCodec::element_record(&image).unwrap();
    let patch = GraphChangeCodec::patch_record(
        &ElementPatch::try_new(image.clone(), BTreeSet::new()).unwrap(),
    )
    .unwrap();
    let partial =
        GraphChangeCodec::partial_record(&ElementBeforeImage::Metadata(metadata("n", &[], 1)))
            .unwrap();

    assert!(matches!(
        GraphChangeCodec::decode_element(&patch),
        Err(GraphCodecError::Operation)
    ));
    assert!(matches!(
        GraphChangeCodec::decode_element(&partial),
        Err(GraphCodecError::Operation)
    ));
    assert!(matches!(
        GraphChangeCodec::decode_patch(&full),
        Err(GraphCodecError::Operation)
    ));
    assert!(matches!(
        GraphChangeCodec::decode_patch(&partial),
        Err(GraphCodecError::Operation)
    ));
    assert!(matches!(
        GraphChangeCodec::decode_partial(&full),
        Err(GraphCodecError::Operation)
    ));
    assert!(matches!(
        GraphChangeCodec::decode_partial(&patch),
        Err(GraphCodecError::Operation)
    ));
}

#[test]
fn patch_roundtrip_distinguishes_set_null_removed_and_omitted() {
    let base = node(
        "n",
        10,
        vec![
            ("keep", ElementValue::Integer(1)),
            ("change", text("before")),
            ("nulled", ElementValue::Integer(3)),
            ("gone", ElementValue::Integer(4)),
        ],
    );
    let image = node_image(
        "n",
        20,
        vec![
            ("change", DataValue::String("after".to_string())),
            ("nulled", DataValue::Null),
            ("added", DataValue::Bool(true)),
        ],
    );
    let patch = ElementPatch::try_new(
        image,
        ["gone", "ghost"].into_iter().map(String::from).collect(),
    )
    .unwrap();

    let record = GraphChangeCodec::patch_record(&patch).unwrap();
    assert_eq!(record.image(), RecordImage::Patch);
    let decoded = GraphChangeCodec::decode_patch(&record).unwrap();
    assert_eq!(decoded, patch);
    assert_eq!(
        decoded.removed_properties().iter().collect::<Vec<_>>(),
        ["ghost", "gone"]
    );

    let result = decoded.apply_to(Some(&base)).unwrap();
    assert_eq!(
        property_names(&result),
        ["added", "change", "keep", "nulled"]
    );
    assert_eq!(
        result.get_properties().get("keep"),
        Some(&ElementValue::Integer(1))
    );
    assert_eq!(result.get_properties().get("change"), Some(&text("after")));
    assert_eq!(
        result.get_properties().get("nulled"),
        Some(&ElementValue::Null)
    );
    assert_eq!(result.get_properties().get("gone"), None);
    assert_eq!(result.get_metadata().effective_from, 20);

    let without_base = decoded.apply_to(None).unwrap();
    assert_eq!(property_names(&without_base), ["added", "change", "nulled"]);
}

#[test]
fn patch_record_rejects_conflicting_set_and_remove() {
    #[derive(Serialize)]
    struct RawPatch {
        image: ElementImage,
        remove: BTreeSet<String>,
    }

    let image = node_image("n", 1, vec![("a", DataValue::Null)]);
    assert!(ElementPatch::try_new(image.clone(), ["a".to_string()].into()).is_err());

    let payload = encode_bounded_messagepack(
        &RawPatch {
            image,
            remove: ["a".to_string()].into(),
        },
        LIMIT,
    )
    .unwrap();
    assert!(Record::try_new(
        &GraphChangeCodec::schema(),
        identity("n"),
        RecordImage::Patch,
        Bytes::from(payload),
    )
    .is_err());
}

#[test]
fn before_images_roundtrip_metadata_only_and_known_element() {
    let metadata_only = ElementBeforeImage::Metadata(metadata("n", &["A", "B"], 77));
    let known = ElementBeforeImage::Element(relation_image(
        "r",
        9,
        vec![("weight", DataValue::Unsigned(3))],
    ));

    for image in [metadata_only, known] {
        let record = GraphChangeCodec::partial_record(&image).unwrap();
        assert_eq!(record.image(), RecordImage::Partial);
        assert_eq!(
            record.reference(),
            &GraphChangeCodec::element_reference(&image.metadata().reference).unwrap()
        );
        assert_eq!(GraphChangeCodec::decode_partial(&record).unwrap(), image);
    }
}

#[test]
fn invalid_values_are_never_encoded() {
    let invalid = node_image(
        "bad",
        1,
        vec![(
            "duration",
            DataValue::Duration {
                seconds: 0,
                nanos: 1_000_000_000,
                years: 0,
                months: 0,
            },
        )],
    );
    assert!(matches!(
        GraphChangeCodec::element_record(&invalid),
        Err(GraphCodecError::Image(_))
    ));
    assert!(matches!(
        GraphChangeCodec::partial_record(&ElementBeforeImage::Element(invalid.clone())),
        Err(GraphCodecError::Image(_))
    ));

    let payload = encode_bounded_messagepack(&invalid, LIMIT).unwrap();
    assert!(Record::try_new(
        &GraphChangeCodec::schema(),
        identity("bad"),
        RecordImage::Full,
        Bytes::from(payload),
    )
    .is_err());
}

#[test]
fn records_reject_truncated_trailing_and_malformed_payloads() {
    let image = node_image("n", 1, vec![("a", DataValue::Bool(true))]);
    let valid = [
        (
            RecordImage::Full,
            GraphChangeCodec::element_record(&image)
                .unwrap()
                .payload()
                .to_vec(),
        ),
        (
            RecordImage::Patch,
            GraphChangeCodec::patch_record(
                &ElementPatch::try_new(image.clone(), ["gone".to_string()].into()).unwrap(),
            )
            .unwrap()
            .payload()
            .to_vec(),
        ),
        (
            RecordImage::Partial,
            GraphChangeCodec::partial_record(&ElementBeforeImage::Element(image))
                .unwrap()
                .payload()
                .to_vec(),
        ),
    ];
    let schema = GraphChangeCodec::schema();
    let accepts = |image: RecordImage, payload: Vec<u8>| {
        Record::try_new(&schema, identity("n"), image, Bytes::from(payload)).is_ok()
    };

    for (kind, payload) in valid {
        assert!(accepts(kind, payload.clone()));
        for end in [0, 1, payload.len() / 2, payload.len() - 1] {
            assert!(!accepts(kind, payload[..end].to_vec()), "{kind:?} {end}");
        }
        let mut trailing = payload.clone();
        trailing.push(0xc0);
        assert!(!accepts(kind, trailing));
        let mut doubled = payload.clone();
        doubled.extend_from_slice(&payload);
        assert!(!accepts(kind, doubled));
        assert!(!accepts(kind, vec![0xc1]));
        assert!(!accepts(kind, vec![0xdd, 0xff, 0xff, 0xff, 0xff]));
    }
}

#[test]
fn records_reject_mismatched_identity_and_image_kind() {
    let image = node_image("n1", 1, vec![]);
    let schema = GraphChangeCodec::schema();
    let full = GraphChangeCodec::element_record(&image).unwrap();
    let patch = GraphChangeCodec::patch_record(
        &ElementPatch::try_new(image.clone(), BTreeSet::new()).unwrap(),
    )
    .unwrap();
    let partial = GraphChangeCodec::partial_record(&ElementBeforeImage::Element(image)).unwrap();
    let other_source =
        GraphChangeCodec::element_reference(&ElementReference::new("other", "n1")).unwrap();

    for record in [&full, &patch, &partial] {
        for wrong in [identity("n2"), other_source.identity().clone()] {
            assert!(
                Record::try_new(&schema, wrong, record.image(), record.payload().clone()).is_err()
            );
        }
    }

    for (record, wrong) in [
        (&full, [RecordImage::Patch, RecordImage::Partial]),
        (&patch, [RecordImage::Full, RecordImage::Partial]),
        (&partial, [RecordImage::Full, RecordImage::Patch]),
    ] {
        for image in wrong {
            assert!(
                Record::try_new(&schema, identity("n1"), image, record.payload().clone()).is_err()
            );
        }
    }
}

#[test]
fn record_identity_must_be_a_canonical_graph_reference() {
    let schema = GraphChangeCodec::schema();
    let canonical = identity("n");
    let value = canonical.value().clone();

    assert!(RecordReference::try_new(&schema, canonical).is_ok());
    assert!(RecordReference::try_new(
        &schema,
        RecordId::try_new("other.identity", value.clone()).unwrap()
    )
    .is_err());
    let mut trailing = value.to_vec();
    trailing.push(0xc0);
    assert!(RecordReference::try_new(
        &schema,
        RecordId::try_new(GRAPH_IDENTITY, Bytes::from(trailing)).unwrap()
    )
    .is_err());
    assert!(RecordReference::try_new(
        &schema,
        RecordId::try_new(GRAPH_IDENTITY, value.slice(..value.len() - 1)).unwrap()
    )
    .is_err());
    assert!(RecordReference::try_new(
        &schema,
        RecordId::try_new(GRAPH_IDENTITY, Bytes::from_static(&[0xc1])).unwrap()
    )
    .is_err());
}

#[test]
fn decoders_verify_the_exact_schema_and_reference_of_the_record() {
    let image = node_image("n1", 1, vec![]);
    let canonical = [
        GraphChangeCodec::element_record(&image).unwrap(),
        GraphChangeCodec::patch_record(
            &ElementPatch::try_new(image.clone(), BTreeSet::new()).unwrap(),
        )
        .unwrap(),
        GraphChangeCodec::partial_record(&ElementBeforeImage::Element(image)).unwrap(),
    ];

    let lenient = lenient_graph_schema();
    let foreign = Schema::new(foreign_descriptor("other.schema", 1), Arc::new(AcceptAll));
    let newer = Schema::new(
        foreign_descriptor("drasi.graph-change", 2),
        Arc::new(AcceptAll),
    );
    for record in &canonical {
        assert!(decode_record(record).is_ok());
        let kind = record.image();
        let payload = record.payload().clone();

        // Another element's identity under the graph schema.
        let wrong_element =
            Record::try_new(&lenient, identity("n2"), kind, payload.clone()).unwrap();
        assert!(matches!(
            decode_record(&wrong_element),
            Err(GraphCodecError::Operation)
        ));

        // The right element bytes under another identity namespace.
        let wrong_namespace = Record::try_new(
            &lenient,
            RecordId::try_new("other.identity", identity("n1").value().clone()).unwrap(),
            kind,
            payload.clone(),
        )
        .unwrap();
        assert!(matches!(
            decode_record(&wrong_namespace),
            Err(GraphCodecError::Operation)
        ));

        // A different schema id or version is never read as graph data.
        for schema in [&foreign, &newer] {
            let record = Record::try_new(schema, identity("n1"), kind, payload.clone()).unwrap();
            assert!(matches!(
                decode_record(&record),
                Err(GraphCodecError::Operation)
            ));
        }
    }
}

#[test]
fn decode_changes_rejects_other_schemas_and_noncanonical_records() {
    let image = node_image("n1", 1, vec![]);
    let payload = GraphChangeCodec::element_record(&image)
        .unwrap()
        .payload()
        .clone();

    // Same descriptor, but a record whose identity does not match its payload.
    let mismatched = Record::try_new(
        &lenient_graph_schema(),
        identity("n2"),
        RecordImage::Full,
        payload.clone(),
    )
    .unwrap();
    let bad = envelope(
        vec![ChangeOperation::Added {
            ordinal: 0,
            after: mismatched,
        }],
        None,
    );
    assert!(GraphChangeCodec::decode_changes(&bad).is_err());

    for descriptor in [
        foreign_descriptor("other.schema", 1),
        foreign_descriptor("drasi.graph-change", 2),
    ] {
        let schema = Schema::new(descriptor.clone(), Arc::new(AcceptAll));
        let record =
            Record::try_new(&schema, identity("n1"), RecordImage::Full, payload.clone()).unwrap();
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new("test", Bytes::from_static(&[1])).unwrap(),
            descriptor.clone(),
            vec![ChangeOperation::Added {
                ordinal: 0,
                after: record,
            }],
        )
        .unwrap();
        let data_envelope = ChangeEnvelope::new(
            crate::emission_id(&stream("test"), 1).unwrap(),
            changes,
            SystemMetadata::new(stream("test"), 1),
        );
        assert!(matches!(
            GraphChangeCodec::decode_changes(&data_envelope),
            Err(GraphCodecError::Contract(
                ContractError::SchemaMismatch { .. }
            ))
        ));

        let empty = ChangeSet::try_new(
            ChangeSetId::try_new("test", Bytes::from_static(&[2])).unwrap(),
            descriptor,
            Vec::new(),
        )
        .unwrap();
        let due = ChangeEnvelope::new(
            crate::emission_id(&stream("test"), 2).unwrap(),
            empty,
            SystemMetadata::new(stream("test"), 2).with_control(
                ControlMessage::try_new(GRAPH_CONTROL, encode(&GraphControl::FuturesDue).unwrap())
                    .unwrap(),
            ),
        );
        assert!(GraphChangeCodec::decode_changes(&due).is_err());
    }

    // The graph validator refuses to validate under a foreign descriptor.
    let foreign = Schema::new(
        foreign_descriptor("other.schema", 1),
        Arc::new(GraphValidator),
    );
    assert!(RecordReference::try_new(&foreign, identity("n1")).is_err());
}

#[test]
fn replacement_decodes_as_one_full_replace() {
    let old = node_image("n", 1, vec![("a", DataValue::String("old".to_string()))]);
    let new = node_image("n", 2, vec![("b", DataValue::String("new".to_string()))]);
    let expected = Element::try_from(&new).unwrap();

    for before in [None, Some(GraphChangeCodec::element_record(&old).unwrap())] {
        let replacement = envelope(
            vec![ChangeOperation::Updated {
                ordinal: 0,
                before,
                after: GraphChangeCodec::element_record(&new).unwrap(),
                semantics: UpdateSemantics::Replace,
            }],
            None,
        );
        for decoded in std::iter::once(replacement.clone()).chain(through_codecs(&replacement)) {
            assert_eq!(
                GraphChangeCodec::decode_changes(&decoded).unwrap(),
                [SourceChange::Replace {
                    element: expected.clone()
                }]
            );
        }
    }

    // A replacement image must be full, not a patch.
    let patch =
        GraphChangeCodec::patch_record(&ElementPatch::try_new(new, BTreeSet::new()).unwrap())
            .unwrap();
    assert!(ChangeSet::try_new(
        ChangeSetId::try_new("test", Bytes::from_static(&[1])).unwrap(),
        GraphChangeCodec::schema().descriptor().clone(),
        vec![ChangeOperation::Updated {
            ordinal: 0,
            before: None,
            after: patch,
            semantics: UpdateSemantics::Replace,
        }],
    )
    .is_err());
}

#[test]
fn identity_only_delete_requires_a_nonnegative_event_time() {
    let delete =
        GraphChangeCodec::decode_changes(&envelope(vec![deleted(0, "n", None)], Some(at(1_234))))
            .unwrap();
    assert_eq!(
        delete,
        [SourceChange::Delete {
            metadata: ElementMetadata {
                reference: reference("n"),
                labels: Arc::from([]),
                effective_from: 1_234,
            }
        }]
    );

    let epoch =
        GraphChangeCodec::decode_changes(&envelope(vec![deleted(0, "n", None)], Some(at(0))))
            .unwrap();
    assert!(matches!(
        &epoch[..],
        [SourceChange::Delete { metadata }] if metadata.effective_from == 0
    ));

    for timestamp in [None, Some(at(-1))] {
        assert!(matches!(
            GraphChangeCodec::decode_changes(&envelope(vec![deleted(0, "n", None)], timestamp)),
            Err(GraphCodecError::MissingEventTime)
        ));
    }
}

#[test]
fn metadata_only_delete_keeps_its_precise_effective_time() {
    let precise = metadata("n", &["A", "B"], 42);
    let record =
        GraphChangeCodec::partial_record(&ElementBeforeImage::Metadata(precise.clone())).unwrap();

    for timestamp in [None, Some(at(-5)), Some(at(9_000))] {
        let delete = envelope(vec![deleted(0, "n", Some(record.clone()))], timestamp);
        assert_eq!(
            GraphChangeCodec::decode_changes(&delete).unwrap(),
            [SourceChange::Delete {
                metadata: precise.clone()
            }]
        );
    }
}

#[test]
fn full_and_element_before_images_use_the_envelope_time_for_deletes() {
    let image = node_image("n", 7, vec![("a", DataValue::Bool(true))]);
    let records = [
        GraphChangeCodec::element_record(&image).unwrap(),
        GraphChangeCodec::partial_record(&ElementBeforeImage::Element(image.clone())).unwrap(),
    ];

    for record in records {
        let decoded = GraphChangeCodec::decode_changes(&envelope(
            vec![deleted(0, "n", Some(record.clone()))],
            Some(at(9_000)),
        ))
        .unwrap();
        assert_eq!(
            decoded,
            [SourceChange::Delete {
                metadata: ElementMetadata {
                    effective_from: 9_000,
                    ..image.metadata().clone()
                }
            }]
        );

        for timestamp in [None, Some(at(-1))] {
            assert!(matches!(
                GraphChangeCodec::decode_changes(&envelope(
                    vec![deleted(0, "n", Some(record.clone()))],
                    timestamp
                )),
                Err(GraphCodecError::MissingEventTime)
            ));
        }
    }
}

#[test]
fn delete_before_images_must_match_the_deleted_identity_and_not_be_patches() {
    let schema = GraphChangeCodec::schema().descriptor().clone();
    let id = || ChangeSetId::try_new("test", Bytes::from_static(&[1])).unwrap();
    let image = node_image("n", 1, vec![]);

    let other = GraphChangeCodec::element_record(&node_image("other", 1, vec![])).unwrap();
    assert!(ChangeSet::try_new(id(), schema.clone(), vec![deleted(0, "n", Some(other))]).is_err());

    let patch =
        GraphChangeCodec::patch_record(&ElementPatch::try_new(image, BTreeSet::new()).unwrap())
            .unwrap();
    assert!(ChangeSet::try_new(id(), schema, vec![deleted(0, "n", Some(patch))]).is_err());
}

#[test]
fn update_carries_set_null_and_removed_properties_through_both_codecs() {
    let element = node(
        "n",
        5,
        vec![("kept", text("v")), ("null", ElementValue::Null)],
    );
    let update = GraphChangeCodec::encode_changes(
        &[SourceChange::update(element.clone())],
        stream("test"),
        1,
        None,
    )
    .unwrap();
    let [ChangeOperation::Updated {
        before,
        after,
        semantics,
        ..
    }] = update.changes().operations()
    else {
        panic!("expected one update");
    };
    assert!(before.is_none());
    assert_eq!(*semantics, UpdateSemantics::Patch);
    assert!(GraphChangeCodec::decode_patch(after)
        .unwrap()
        .removed_properties()
        .is_empty());
    assert_eq!(
        GraphChangeCodec::decode_changes(&update).unwrap(),
        [SourceChange::update(element)]
    );

    let removing = SourceChange::update_with_removals(
        node("n", 5, vec![("null", ElementValue::Null)]),
        ["gone".to_string()].into(),
    )
    .unwrap();
    let encoded =
        GraphChangeCodec::encode_changes(std::slice::from_ref(&removing), stream("test"), 2, None)
            .unwrap();
    let [ChangeOperation::Updated { after, .. }] = encoded.changes().operations() else {
        panic!("expected one update");
    };
    // The canonical patch and the processing change keep the same removal set.
    assert!(GraphChangeCodec::decode_patch(after)
        .unwrap()
        .removed_properties()
        .contains("gone"));
    for decoded in std::iter::once(encoded.clone()).chain(through_codecs(&encoded)) {
        assert_eq!(
            GraphChangeCodec::decode_changes(&decoded)
                .unwrap()
                .as_slice(),
            std::slice::from_ref(&removing)
        );
    }

    let removal =
        ElementPatch::try_new(node_image("n", 5, vec![]), ["gone".to_string()].into()).unwrap();
    let canonical = envelope_of_update(GraphChangeCodec::patch_record(&removal).unwrap());
    for decoded in std::iter::once(canonical.clone()).chain(through_codecs(&canonical)) {
        let changes = GraphChangeCodec::decode_changes(&decoded).unwrap();
        let [SourceChange::Update {
            element,
            removed_properties,
        }] = changes.as_slice()
        else {
            panic!("expected one update with removals");
        };
        assert!(element.get_properties().get("gone").is_none());
        assert_eq!(
            removed_properties.iter().collect::<Vec<_>>(),
            ["gone"].iter().collect::<Vec<_>>()
        );
    }
}

#[test]
fn update_that_sets_and_removes_a_property_is_rejected_when_encoding() {
    let conflicting = SourceChange::Update {
        element: node("n", 5, vec![("both", ElementValue::Null)]),
        removed_properties: ["both".to_string()].into(),
    };
    assert!(matches!(
        GraphChangeCodec::encode_changes(&[conflicting], stream("test"), 1, None),
        Err(GraphCodecError::Patch(_))
    ));
}

#[test]
fn replace_stays_a_full_replacement_through_both_codecs() {
    let replacement = SourceChange::replace(node("n", 5, vec![("kept", text("v"))]));
    let encoded = GraphChangeCodec::encode_changes(
        std::slice::from_ref(&replacement),
        stream("test"),
        1,
        None,
    )
    .unwrap();
    let [ChangeOperation::Updated {
        before,
        after,
        semantics,
        ..
    }] = encoded.changes().operations()
    else {
        panic!("a replacement is one update, not an add or a delete and insert");
    };
    assert!(before.is_none());
    assert_eq!(*semantics, UpdateSemantics::Replace);
    assert_eq!(after.image(), RecordImage::Full);
    for decoded in std::iter::once(encoded.clone()).chain(through_codecs(&encoded)) {
        assert_eq!(
            GraphChangeCodec::decode_changes(&decoded)
                .unwrap()
                .as_slice(),
            std::slice::from_ref(&replacement)
        );
    }

    // Re-encoding the decoded change keeps the replacement, so a middleware pass
    // cannot turn it into an addition.
    let decoded = GraphChangeCodec::decode_changes(&encoded).unwrap();
    let again = GraphChangeCodec::derive_changes(&encoded, &decoded, stream("test"), 2).unwrap();
    assert!(matches!(
        again.changes().operations(),
        [ChangeOperation::Updated {
            semantics: UpdateSemantics::Replace,
            ..
        }]
    ));
}

#[test]
fn mixed_changes_keep_original_order_through_both_codecs() {
    let changes = mixed_changes();
    let original =
        GraphChangeCodec::encode_changes(&changes, stream("test"), 7, Some(at(5_000))).unwrap();

    // Operations are data only: futures are not operations and ordinals keep their gaps.
    let operations = original.changes().operations();
    assert_eq!(
        operations
            .iter()
            .map(ChangeOperation::ordinal)
            .collect::<Vec<_>>(),
        [0, 2, 4]
    );
    assert!(matches!(
        operations,
        [
            ChangeOperation::Added { .. },
            ChangeOperation::Updated {
                semantics: UpdateSemantics::Patch,
                ..
            },
            ChangeOperation::Deleted { .. }
        ]
    ));
    assert_eq!(
        reevaluations(&original),
        [(1, future("a", 100)), (3, future("b", 200))]
    );
    assert_eq!(
        original.system().control().map(ControlMessage::kind),
        Some(GRAPH_CONTROL)
    );
    assert!(!GraphChangeCodec::is_futures_due(&original));

    for decoded in std::iter::once(original.clone()).chain(through_codecs(&original)) {
        assert_eq!(
            decoded.system().control(),
            original.system().control(),
            "control survives the codec unchanged"
        );
        assert_eq!(GraphChangeCodec::decode_changes(&decoded).unwrap(), changes);
    }
}

#[test]
fn leading_trailing_and_adjacent_futures_keep_their_position() {
    let insert = |id: &str| SourceChange::Insert {
        element: node(id, 1, vec![]),
    };
    let wake = |id: &str| SourceChange::Future {
        future_ref: future(id, 10),
    };
    for changes in [
        vec![wake("f"), insert("a")],
        vec![insert("a"), wake("f")],
        vec![wake("f1"), wake("f2"), insert("a"), wake("f3")],
        vec![insert("a"), wake("f1"), wake("f2"), insert("b")],
    ] {
        let original = GraphChangeCodec::encode_changes(&changes, stream("test"), 1, None).unwrap();
        for decoded in through_codecs(&original) {
            assert_eq!(GraphChangeCodec::decode_changes(&decoded).unwrap(), changes);
        }
    }
}

#[test]
fn future_only_controls_have_no_data_operations() {
    let changes = [
        SourceChange::Future {
            future_ref: future("a", 10),
        },
        SourceChange::Future {
            future_ref: future("b", 20),
        },
    ];
    let original = GraphChangeCodec::encode_changes(&changes, stream("test"), 3, None).unwrap();
    assert!(original.changes().operations().is_empty());
    assert_eq!(
        reevaluations(&original),
        [(0, future("a", 10)), (1, future("b", 20))]
    );

    for decoded in through_codecs(&original) {
        assert!(decoded.changes().operations().is_empty());
        assert!(!GraphChangeCodec::is_futures_due(&decoded));
        assert_eq!(GraphChangeCodec::decode_changes(&decoded).unwrap(), changes);
    }
}

#[test]
fn due_control_survives_codecs_and_reemit_without_data() {
    let due = GraphChangeCodec::encode_futures_due(stream("timer"), 4, at(8_000)).unwrap();
    assert!(due.changes().operations().is_empty());
    assert!(GraphChangeCodec::is_futures_due(&due));
    assert!(GraphChangeCodec::decode_changes(&due).unwrap().is_empty());

    for decoded in through_codecs(&due) {
        assert!(GraphChangeCodec::is_futures_due(&decoded));
        assert!(GraphChangeCodec::decode_changes(&decoded)
            .unwrap()
            .is_empty());
        assert_eq!(decoded.system().timestamp(), Some(at(8_000)));

        let replayed = decoded.reemit(11).unwrap();
        assert_eq!(replayed.system().sequence(), 11);
        assert_eq!(replayed.system().control(), due.system().control());
        assert!(GraphChangeCodec::is_futures_due(&replayed));
    }

    let wake = GraphChangeCodec::encode_changes(
        &[SourceChange::Future {
            future_ref: future("a", 1),
        }],
        stream("test"),
        1,
        None,
    )
    .unwrap();
    let replayed = wake.reemit(2).unwrap();
    assert_eq!(replayed.system().control(), wake.system().control());
    assert_eq!(
        GraphChangeCodec::decode_changes(&replayed).unwrap(),
        GraphChangeCodec::decode_changes(&wake).unwrap()
    );
}

#[test]
fn derived_output_is_not_retriggered_by_inherited_due_state() {
    let mut due = GraphChangeCodec::encode_futures_due(stream("timer"), 4, at(8_000)).unwrap();
    due.append_annotation(
        ContextEntry::try_new(
            ComponentId::try_new("query").unwrap(),
            "drasi.query.futures-due",
            ContextValue::Bool(true),
        )
        .unwrap(),
    )
    .unwrap();

    let derived = GraphChangeCodec::derive_changes(&due, &[], stream("out"), 3).unwrap();
    assert!(derived.changes().operations().is_empty());
    assert!(derived.system().control().is_none());
    assert!(!GraphChangeCodec::is_futures_due(&derived));
    assert!(GraphChangeCodec::decode_changes(&derived)
        .unwrap()
        .is_empty());
    assert!(derived.annotations().entries().any(|entry| {
        entry.key() == "drasi.query.futures-due" && entry.value() == ContextValue::Bool(true)
    }));
    assert_eq!(derived.system().timestamp(), Some(at(8_000)));

    // Only the output's own futures produce a control, numbered for the output.
    let output = [
        SourceChange::Future {
            future_ref: future("a", 1),
        },
        SourceChange::Insert {
            element: node("a", 1, vec![]),
        },
    ];
    let derived = GraphChangeCodec::derive_changes(&due, &output, stream("out"), 4).unwrap();
    assert!(!GraphChangeCodec::is_futures_due(&derived));
    assert_eq!(reevaluations(&derived), [(0, future("a", 1))]);
    assert_eq!(derived.changes().operations()[0].ordinal(), 1);
    assert_eq!(GraphChangeCodec::decode_changes(&derived).unwrap(), output);
}

#[test]
fn encode_defaults_to_event_time_and_derive_preserves_input_ordering() {
    let output = [SourceChange::Insert {
        element: node("a", 1, vec![]),
    }];
    let fresh =
        GraphChangeCodec::encode_changes(&output, stream("test"), 1, Some(at(1_000))).unwrap();
    assert_eq!(fresh.system().ordering(), ChangeOrdering::EventTime);
    assert_eq!(
        GraphChangeCodec::encode_change(output[0].clone(), stream("test"), 2, None)
            .unwrap()
            .system()
            .ordering(),
        ChangeOrdering::EventTime
    );

    for ordering in [ChangeOrdering::EventTime, ChangeOrdering::ProducerSequence] {
        let input = ChangeEnvelope::new(
            crate::emission_id(&stream("in"), 7).unwrap(),
            fresh.changes().clone(),
            SystemMetadata::new(stream("in"), 7)
                .with_timestamp(at(2_000))
                .with_ordering(ordering),
        );
        let derived = GraphChangeCodec::derive_changes(&input, &output, stream("out"), 1).unwrap();
        assert_eq!(derived.system().ordering(), ordering);
        assert_eq!(derived.system().timestamp(), Some(at(2_000)));
        assert_eq!(derived.lineage().unwrap().system().ordering(), ordering);
    }
}

#[test]
fn malformed_control_kind_or_payload_is_rejected() {
    let base = envelope(vec![added(0, &node_image("a", 1, vec![]))], None);
    let valid = reevaluate(vec![(1, future("f", 1))]);

    let mut truncated = valid.payload().to_vec();
    truncated.pop();
    let mut trailing = valid.payload().to_vec();
    trailing.push(0xc0);
    let unknown_variant = encode(&"Unknown").unwrap().to_vec();
    let payloadless = encode(&"Reevaluate").unwrap().to_vec();

    assert!(GraphChangeCodec::decode_changes(&with_control(&base, Some(valid.clone()))).is_ok());
    assert_control_error(&with_control(
        &base,
        Some(control_with_payload(
            "drasi.other-control",
            valid.payload().to_vec(),
        )),
    ));
    for payload in [
        Vec::new(),
        vec![0xc1],
        truncated,
        trailing,
        unknown_variant,
        payloadless,
    ] {
        let malformed = with_control(&base, Some(control_with_payload(GRAPH_CONTROL, payload)));
        assert!(matches!(
            GraphChangeCodec::decode_changes(&malformed),
            Err(GraphCodecError::Encoding(_))
        ));
    }
}

#[test]
fn invalid_reevaluation_ordinals_are_rejected() {
    let data_at = |ordinals: &[u64]| {
        envelope(
            ordinals
                .iter()
                .map(|ordinal| added(*ordinal, &node_image(&format!("n{ordinal}"), 1, vec![])))
                .collect(),
            None,
        )
    };
    let controls = |ordinals: &[u64]| {
        reevaluate(
            ordinals
                .iter()
                .map(|ordinal| (*ordinal, future(&format!("f{ordinal}"), 1)))
                .collect(),
        )
    };

    // (data ordinals, reevaluation ordinals): none forms one complete 0..n batch.
    for (operations, futures) in [
        (vec![], vec![]),
        (vec![], vec![1]),
        (vec![], vec![0, 0]),
        (vec![0], vec![0]),
        (vec![0, 1], vec![1]),
        (vec![0], vec![2]),
        (vec![0, 2], vec![3]),
        (vec![1], vec![2]),
        (vec![0], vec![1, 1]),
    ] {
        assert_control_error(&with_control(
            &data_at(&operations),
            Some(controls(&futures)),
        ));
    }

    // Complete batches still decode, in ordinal order.
    let complete = with_control(&data_at(&[0, 2]), Some(controls(&[1, 3])));
    assert_eq!(
        GraphChangeCodec::decode_changes(&complete).unwrap().len(),
        4
    );
}

#[test]
fn reevaluation_ordinals_must_be_strictly_ascending() {
    let base = envelope(vec![added(0, &node_image("a", 1, vec![]))], None);
    let out_of_order = reevaluate(vec![(2, future("f2", 1)), (1, future("f1", 1))]);
    assert_control_error(&with_control(&base, Some(out_of_order)));
}

#[test]
fn empty_reevaluation_list_is_rejected() {
    let empty = envelope(Vec::new(), None);
    assert_control_error(&with_control(&empty, Some(reevaluate(Vec::new()))));
    let with_data = envelope(vec![added(0, &node_image("a", 1, vec![]))], None);
    assert_control_error(&with_control(&with_data, Some(reevaluate(Vec::new()))));
}

#[test]
fn due_control_cannot_carry_data() {
    let due = GraphChangeCodec::encode_futures_due(stream("test"), 1, at(1)).unwrap();
    let data = envelope(vec![added(0, &node_image("a", 1, vec![]))], Some(at(1)));
    let combined = with_control(&data, due.system().control().cloned());

    assert!(!GraphChangeCodec::is_futures_due(&combined));
    assert_control_error(&combined);
    for decoded in through_codecs(&combined) {
        assert_control_error(&decoded);
    }
}

#[test]
fn legacy_bincode_source_change_payloads_are_not_read() {
    let legacy = bincode::serialize(&SourceChange::Insert {
        element: node("n", 1, vec![("a", text("v"))]),
    })
    .unwrap();
    let images = [RecordImage::Full, RecordImage::Patch, RecordImage::Partial];

    let schema = GraphChangeCodec::schema();
    let lenient = lenient_graph_schema();
    for image in images {
        assert!(
            Record::try_new(&schema, identity("n"), image, Bytes::from(legacy.clone())).is_err(),
            "{image:?}"
        );
        // Even a record that skipped payload validation cannot be decoded.
        let record =
            Record::try_new(&lenient, identity("n"), image, Bytes::from(legacy.clone())).unwrap();
        assert!(decode_record(&record).is_err(), "{image:?}");
    }
}

#[test]
fn envelope_formats_are_version_one_only() {
    assert_eq!(BinaryEnvelopeCodec::VERSION, 1);
    let original = GraphChangeCodec::encode_changes(
        &[SourceChange::Insert {
            element: node("n", 1, vec![]),
        }],
        stream("test"),
        1,
        None,
    )
    .unwrap();

    let binary = binary_codec();
    let bytes = binary.encode(&original).unwrap();
    assert!(binary.decode(&bytes).is_ok());
    let newer = replace_once(&bytes, b"\xa6format\x01", b"\xa6format\x02");
    assert!(matches!(
        binary.decode(&newer),
        Err(BinaryEnvelopeCodecError::Version(2))
    ));

    let json = json_codec();
    let bytes = json.encode(&original).unwrap();
    assert!(json.decode(&bytes).is_ok());
    let newer = replace_once(&bytes, b"\"format\":1", b"\"format\":2");
    assert!(matches!(
        json.decode(&newer),
        Err(crate::EnvelopeCodecError::Version(2))
    ));
}
