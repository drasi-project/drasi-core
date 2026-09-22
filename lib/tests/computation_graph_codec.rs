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
use drasi_core::{
    interface::FutureElementRef,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
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
        sequence: 42,
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
        1,
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
        GraphChangeCodec::source_metadata(&envelope)
            .expect("source metadata")
            .expect("raw metadata present")
            .sequence,
        Some(1)
    );
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

#[test]
fn graph_batches_preserve_operation_order_repeated_identities_and_empty_progress() {
    let metadata = ElementMetadata {
        reference: ElementReference::new("source", "item"),
        labels: Arc::from([Arc::from("Item")]),
        effective_from: 1,
    };
    let changes = [
        SourceChange::Insert {
            element: Element::Node {
                metadata: metadata.clone(),
                properties: ElementPropertyMap::from(serde_json::json!({"value":1})),
            },
        },
        SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    effective_from: 2,
                    ..metadata.clone()
                },
                properties: ElementPropertyMap::from(serde_json::json!({"value":2})),
            },
        },
        SourceChange::Future {
            future_ref: FutureElementRef {
                element_ref: metadata.reference.clone(),
                original_time: 2,
                due_time: 3,
                group_signature: 42,
            },
        },
        SourceChange::Delete {
            metadata: ElementMetadata {
                effective_from: 4,
                ..metadata
            },
        },
    ];
    let stream = StreamId::try_new("source/out").expect("stream");
    let timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 123).expect("time");
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024).expect("limit"));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("graph schema");

    for batch in [&changes[..], &[]] {
        let envelope = GraphChangeCodec::encode_changes(batch, stream.clone(), 9, Some(timestamp))
            .expect("batch");
        let decoded = codec
            .decode(&codec.encode(&envelope).expect("encode"))
            .expect("decode envelope");
        assert_eq!(decoded.system().stream(), &stream);
        assert_eq!(decoded.system().sequence(), 9);
        assert_eq!(decoded.system().timestamp(), Some(timestamp));
        assert_eq!(
            decoded
                .changes()
                .operations()
                .iter()
                .map(ChangeOperation::ordinal)
                .collect::<Vec<_>>(),
            (0..batch.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(
            GraphChangeCodec::decode_changes(&decoded).expect("changes"),
            batch
        );
    }

    for change in &changes {
        let single =
            GraphChangeCodec::encode_change(change.clone(), stream.clone(), 9, Some(timestamp))
                .expect("single");
        let batch = GraphChangeCodec::encode_changes(
            std::slice::from_ref(change),
            stream.clone(),
            9,
            Some(timestamp),
        )
        .expect("single-item batch");
        assert_eq!(
            codec.encode(&single).expect("single encoding"),
            codec.encode(&batch).expect("batch encoding")
        );
    }
}

#[test]
fn graph_replacements_delete_the_previous_labels_at_the_new_effective_time() {
    let previous = ElementMetadata {
        reference: ElementReference::new("source", "parent"),
        labels: Arc::from([Arc::from("Previous")]),
        effective_from: 100,
    };
    let replacement = Element::Node {
        metadata: ElementMetadata {
            labels: Arc::from([Arc::from("Replacement")]),
            effective_from: 200,
            ..previous.clone()
        },
        properties: ElementPropertyMap::new(),
    };
    let record = |change: SourceChange| {
        let envelope = GraphChangeCodec::encode_change(
            change,
            StreamId::try_new("source/out").expect("stream"),
            1,
            None,
        )
        .expect("encode record");
        match &envelope.changes().operations()[0] {
            ChangeOperation::Added { after, .. } => after.clone(),
            ChangeOperation::Deleted {
                before: Some(before),
                ..
            } => before.clone(),
            _ => panic!("full or partial record"),
        }
    };
    let after = record(SourceChange::Insert {
        element: replacement.clone(),
    });
    let before_images = [
        Some(record(SourceChange::Insert {
            element: Element::Node {
                metadata: previous.clone(),
                properties: ElementPropertyMap::new(),
            },
        })),
        Some(record(SourceChange::Delete {
            metadata: previous.clone(),
        })),
        None,
    ];

    for before in before_images {
        let expected_metadata = ElementMetadata {
            effective_from: 200,
            ..if before.is_some() {
                previous.clone()
            } else {
                replacement.get_metadata().clone()
            }
        };
        let stream = StreamId::try_new("source/out").expect("stream");
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new("replace", Bytes::from_static(b"parent")).expect("change ID"),
            GraphChangeCodec::schema().descriptor().clone(),
            vec![ChangeOperation::Updated {
                ordinal: 0,
                before,
                after: after.clone(),
                semantics: UpdateSemantics::Replace,
            }],
        )
        .expect("replacement");
        let envelope = ChangeEnvelope::new(
            emission_id(&stream, 2).expect("emission ID"),
            changes,
            SystemMetadata::new(stream, 2),
        );
        assert_eq!(
            GraphChangeCodec::decode_changes(&envelope).expect("decode"),
            [
                SourceChange::Delete {
                    metadata: expected_metadata,
                },
                SourceChange::Insert {
                    element: replacement.clone(),
                },
            ]
        );
    }
}
