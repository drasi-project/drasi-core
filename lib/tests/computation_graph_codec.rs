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

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use chrono::TimeZone;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::{
    channels::{SourceEvent, SourceEventWrapper},
    computation::v1::*,
    profiling::ProfilingMetadata,
    schema::SourceSchema,
};
use ordered_float::OrderedFloat;

#[test]
fn legacy_graph_codec_preserves_all_raw_metadata_and_typed_float_datetime_values() {
    let bits = [(-0.0f64).to_bits(), f64::INFINITY.to_bits(), 0x7ff8000000001234];
    let offset = chrono::FixedOffset::east_opt(3600).expect("offset");
    let datetime = offset
        .timestamp_opt(1_700_000_000, 123_456_789)
        .single()
        .expect("datetime");
    let mut properties = ElementPropertyMap::new();
    for (index, value) in bits.iter().enumerate() {
        properties.insert(
            &format!("float{index}"),
            ElementValue::Float(OrderedFloat(f64::from_bits(*value))),
        );
    }
    properties.insert("local", ElementValue::LocalDateTime(datetime.naive_local()));
    properties.insert("zoned", ElementValue::ZonedDateTime(datetime));
    properties.insert(
        "__drasi_v1_type__",
        ElementValue::String(Arc::from("ordinary user property")),
    );
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("raw source", "id:\0opaque"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 1000,
        },
        properties,
    };
    let change = SourceChange::Insert { element };
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 987654321).expect("timestamp");
    let original = Arc::new(SourceEventWrapper {
        source_id: "legacy-source".into(),
        event: SourceEvent::Change(change.clone()),
        timestamp,
        sequence: Some(42),
        source_position: Some(Bytes::from_static(&[0xff, 0, 1])),
        profiling: Some(ProfilingMetadata {
            source_ns: Some(10),
            source_receive_ns: Some(20),
            source_send_ns: Some(30),
            ..Default::default()
        }),
    });
    let envelope = GraphChangeCodec::encode_source_event(
        original.clone(),
        &ComponentId::try_new("adapter").expect("id"),
        StreamId::try_new("adapter/out").expect("stream"),
        1,
        Some(SourceSchema::default()),
    )
    .expect("shared A2 ingress");
    assert_eq!(envelope.system().sequence(), 1);
    assert_eq!(
        envelope.system().source_position(),
        original.source_position.as_ref()
    );
    assert_eq!(envelope.system().timestamp(), Some(timestamp));
    let metadata = GraphChangeCodec::source_metadata(&envelope)
        .expect("metadata")
        .expect("present");
    assert_eq!(metadata.sequence, Some(42));
    assert_eq!(metadata.source_position, Some(vec![0xff, 0, 1]));
    assert_eq!(metadata.profiling, original.profiling);
    assert_eq!(metadata.schema, Some(SourceSchema::default()));
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024).expect("limit"));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("register graph schema");
    let envelope = codec
        .decode(&codec.encode(&envelope).expect("encode"))
        .expect("validated decode");
    let decoded = GraphChangeCodec::decode_changes(&envelope).expect("typed decode");
    assert_eq!(decoded, [change]);
    let SourceChange::Insert {
        element: Element::Node { properties, .. },
    } = &decoded[0]
    else {
        panic!("node");
    };
    for (index, expected) in bits.iter().enumerate() {
        let Some(ElementValue::Float(value)) = properties.get(&format!("float{index}")) else {
            panic!("float");
        };
        assert_eq!(value.0.to_bits(), *expected);
    }
    assert_eq!(
        properties.get("local"),
        Some(&ElementValue::LocalDateTime(datetime.naive_local()))
    );
    assert_eq!(
        properties.get("zoned"),
        Some(&ElementValue::ZonedDateTime(datetime))
    );
    assert_eq!(
        properties.get("__drasi_v1_type__"),
        Some(&ElementValue::String(Arc::from("ordinary user property")))
    );
}

#[test]
fn graph_codec_uses_owned_ingress_and_rejects_corrupt_record_images() {
    let change = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "node"),
            labels: Arc::from([]),
            effective_from: 999,
        },
    };
    let event = Arc::new(SourceEventWrapper::new(
        "source".into(),
        SourceEvent::Change(change.clone()),
        chrono::Utc::now(),
    ));
    let envelope = GraphChangeCodec::encode_source_event(
        event,
        &ComponentId::try_new("adapter").expect("id"),
        StreamId::try_new("adapter/out").expect("stream"),
        9,
        None,
    )
    .expect("owned ingress");
    assert_eq!(
        GraphChangeCodec::decode_changes(&envelope).expect("decode"),
        [change]
    );
    let identity = envelope.changes().operations()[0].identity().clone();
    assert!(Record::try_new(
        &GraphChangeCodec::schema(),
        identity,
        RecordImage::Partial,
        Bytes::from_static(b"not a source change")
    )
    .is_err());
}
