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
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use bytes::Bytes;
use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeZone};
use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        variable_value::{
            float::Float, zoned_datetime::ZonedDateTime as VarZonedDateTime,
            zoned_time::ZonedTime as VarZonedTime, VariableValue,
        },
    },
    interface::FutureElementRef,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
};
use ordered_float::OrderedFloat;
use serde_json::json;

use crate::{
    channels::{ResultDiff, SourceEvent, SourceEventWrapper},
    profiling::ProfilingMetadata,
};

use super::*;

fn element(properties: ElementPropertyMap, effective_from: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source-a", "person-1"),
            labels: vec!["Person".into()].into(),
            effective_from,
        },
        properties,
    }
}

fn wrapper(change: SourceChange) -> SourceEventWrapper {
    let mut wrapper = SourceEventWrapper::with_sequence(
        "source-a".to_string(),
        SourceEvent::Change(change),
        chrono::DateTime::from_timestamp_millis(1_700_000_000_123).unwrap(),
        42,
        Some(ProfilingMetadata {
            source_ns: Some(10),
            source_receive_ns: Some(20),
            source_send_ns: Some(30),
            ..Default::default()
        }),
    );
    wrapper.set_source_position(Bytes::from_static(b"offset-42"));
    wrapper
}

fn variables(entries: &[(&str, VariableValue)]) -> QueryVariables {
    entries
        .iter()
        .map(|(key, value)| ((*key).into(), value.clone()))
        .collect()
}

fn query_metadata(sequence: u64) -> QueryEnvelopeMetadata {
    QueryEnvelopeMetadata::new(
        "query-a",
        Some(Arc::from("source-a")),
        sequence,
        chrono::DateTime::from_timestamp_millis(1_700_000_000_456).unwrap(),
        HashMap::from([
            ("source_id".to_string(), json!("source-a")),
            ("processed_by".to_string(), json!("drasi-core")),
        ]),
        Some(ProfilingMetadata {
            query_receive_ns: Some(40),
            query_core_call_ns: Some(50),
            query_core_return_ns: Some(60),
            query_send_ns: Some(70),
            ..Default::default()
        }),
    )
}

fn assert_source_wrapper_eq(actual: &SourceEventWrapper, expected: &SourceEventWrapper) {
    assert_eq!(actual.source_id, expected.source_id);
    assert_eq!(actual.event, expected.event);
    assert_eq!(actual.timestamp, expected.timestamp);
    assert_eq!(actual.profiling, expected.profiling);
    assert_eq!(actual.sequence, expected.sequence);
    assert_eq!(actual.source_position, expected.source_position);
}

#[test]
fn schema_references_are_versioned_and_distinct() {
    let graph = graph_change_schema();
    let graph_again = graph_change_schema();
    let query = query_result_schema();

    assert!(Arc::ptr_eq(&graph, &graph_again));
    assert_eq!(graph.id(), "drasi.internal.graph-change");
    assert_eq!(graph.version().value(), 1);
    assert_eq!(graph.fingerprint().value(), 0x7763_808c_f677_2102);
    assert_eq!(query.id(), "drasi.internal.query-result");
    assert_eq!(query.version().value(), 1);
    assert_eq!(query.fingerprint().value(), 0x476f_7b05_8973_4c67);
    assert_ne!(graph.fingerprint(), query.fingerprint());
}

#[test]
fn append_only_inputs_use_only_added_records() {
    let graph_input = wrapper(SourceChange::Insert {
        element: element(ElementPropertyMap::from(json!({ "name": "Alice" })), 1_000),
    });
    let graph =
        source_event_to_envelope(&graph_input, SystemMetadataExtensions::default()).unwrap();

    assert_eq!(graph.change_set().added().len(), 1);
    assert!(graph.change_set().updated().is_empty());
    assert!(graph.change_set().deleted().is_empty());
    assert!(Arc::ptr_eq(
        graph.change_set().schema(),
        &graph_change_schema()
    ));

    let query = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[("name", VariableValue::String("Alice".to_string()))]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();

    assert_eq!(query.change_set().added().len(), 1);
    assert!(query.change_set().updated().is_empty());
    assert!(query.change_set().deleted().is_empty());
}

#[test]
fn owned_graph_envelope_path_moves_the_element_without_fallback_cloning() {
    let input = Arc::new(wrapper(SourceChange::Insert {
        element: element(
            ElementPropertyMap::from(json!({
                "payload": {
                    "name": "Alice",
                    "tags": ["one", "two", "three"],
                }
            })),
            1_000,
        ),
    }));
    let original_property_address = match &input.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            std::ptr::from_ref(element.get_property("payload")).addr()
        }
        _ => unreachable!("test input is an insert"),
    };

    let parts = SourceEventWrapper::try_unwrap_arc(input)
        .expect("the sole-owned source wrapper should move into parts");
    let envelope =
        source_event_parts_to_envelope(parts, SystemMetadataExtensions::default()).unwrap();
    assert_eq!(Arc::strong_count(envelope.change_set()), 1);
    let RecordData::Graph(GraphRecord::Element(stored)) = envelope.change_set().added()[0].after()
    else {
        panic!("owned insert should remain a typed element");
    };
    assert_eq!(Arc::strong_count(stored), 1);
    assert_eq!(
        std::ptr::from_ref(stored.get_property("payload")).addr(),
        original_property_address,
        "moving SourceEventParts into the envelope must retain the property allocation"
    );

    let recovered = source_change_from_envelope_owned(envelope).unwrap();
    let SourceChange::Insert { element } = recovered else {
        panic!("owned graph envelope should recover the insert");
    };
    assert_eq!(
        std::ptr::from_ref(element.get_property("payload")).addr(),
        original_property_address,
        "the sole-owned envelope must unwrap its element instead of deep-cloning it"
    );
}

#[test]
fn owned_graph_envelope_path_roundtrips_every_change_variant() {
    let metadata = ElementMetadata {
        reference: ElementReference::new("source-a", "person-1"),
        labels: vec!["Person".into()].into(),
        effective_from: 3_000,
    };
    let changes = [
        SourceChange::Update {
            element: element(ElementPropertyMap::from(json!({ "name": "Alicia" })), 2_000),
        },
        SourceChange::Delete {
            metadata: metadata.clone(),
        },
        SourceChange::Future {
            future_ref: FutureElementRef {
                element_ref: metadata.reference,
                original_time: 3_000,
                due_time: 4_000,
                group_signature: 55,
            },
        },
    ];

    for expected in changes {
        let envelope = source_event_parts_to_envelope(
            wrapper(expected.clone()).into_parts(),
            SystemMetadataExtensions::default(),
        )
        .unwrap();
        assert_eq!(
            source_change_from_envelope_owned(envelope).unwrap(),
            expected
        );
    }
}

#[test]
fn graph_updates_and_deletes_roundtrip_with_explicit_semantics() {
    let update_input = wrapper(SourceChange::Update {
        element: element(ElementPropertyMap::from(json!({ "name": "Alicia" })), 2_000),
    });
    let update =
        source_event_to_envelope(&update_input, SystemMetadataExtensions::default()).unwrap();
    assert_eq!(update.change_set().updated().len(), 1);
    assert_eq!(
        update.change_set().updated()[0].semantics(),
        UpdateSemantics::Patch
    );
    assert!(update.change_set().updated()[0].before().is_none());
    assert_source_wrapper_eq(&source_event_from_envelope(&update).unwrap(), &update_input);

    let metadata = ElementMetadata {
        reference: ElementReference::new("source-a", "person-1"),
        labels: vec!["Person".into()].into(),
        effective_from: 3_000,
    };
    let delete_input = wrapper(SourceChange::Delete {
        metadata: metadata.clone(),
    });
    let delete =
        source_event_to_envelope(&delete_input, SystemMetadataExtensions::default()).unwrap();
    assert_eq!(delete.change_set().deleted().len(), 1);
    assert_eq!(
        delete.change_set().deleted()[0].before(),
        Some(&RecordData::Graph(GraphRecord::Metadata(Arc::new(
            metadata
        ))))
    );
    assert_source_wrapper_eq(&source_event_from_envelope(&delete).unwrap(), &delete_input);

    let future_input = wrapper(SourceChange::Future {
        future_ref: FutureElementRef {
            element_ref: ElementReference::new("source-a", "person-1"),
            original_time: 3_000,
            due_time: 4_000,
            group_signature: 55,
        },
    });
    let future =
        source_event_to_envelope(&future_input, SystemMetadataExtensions::default()).unwrap();
    assert_eq!(
        future.change_set().updated()[0].semantics(),
        UpdateSemantics::Patch
    );
    assert!(matches!(
        future.change_set().updated()[0].after(),
        RecordData::Graph(GraphRecord::Future(_))
    ));
    assert_source_wrapper_eq(&source_event_from_envelope(&future).unwrap(), &future_input);
}

#[test]
fn graph_and_query_values_remain_typed_inside_change_sets() {
    let mut properties = ElementPropertyMap::new();
    properties.insert("infinity", ElementValue::Float(OrderedFloat(f64::INFINITY)));
    properties.insert(
        "local_datetime",
        ElementValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2026, 9, 8)
                .unwrap()
                .and_hms_opt(7, 8, 9)
                .unwrap(),
        ),
    );
    let graph_input = wrapper(SourceChange::Insert {
        element: element(properties, 1_000),
    });
    let graph =
        source_event_to_envelope(&graph_input, SystemMetadataExtensions::default()).unwrap();
    assert_source_wrapper_eq(&source_event_from_envelope(&graph).unwrap(), &graph_input);

    let typed_row = variables(&[
        (
            "date",
            VariableValue::Date(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap()),
        ),
        ("large", VariableValue::Integer(u64::MAX.into())),
    ]);
    let query = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: typed_row.clone(),
            row_signature: 99,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let RecordData::QueryRow(stored) = query.change_set().added()[0].after() else {
        panic!("query rows must stay typed");
    };
    assert_eq!(stored.as_ref(), &typed_row);
}

#[test]
fn query_updates_preserve_order_before_images_and_legacy_shape() {
    let contexts = vec![
        QueryPartEvaluationContext::Adding {
            after: variables(&[("kind", VariableValue::String("added".to_string()))]),
            row_signature: 11,
        },
        QueryPartEvaluationContext::Noop,
        QueryPartEvaluationContext::Updating {
            before: variables(&[("kind", VariableValue::String("before".to_string()))]),
            after: variables(&[("kind", VariableValue::String("after".to_string()))]),
            row_signature: 22,
        },
        QueryPartEvaluationContext::Removing {
            before: variables(&[("kind", VariableValue::String("removed".to_string()))]),
            row_signature: 33,
        },
        QueryPartEvaluationContext::Aggregation {
            before: Some(variables(&[("total", VariableValue::Integer(1.into()))])),
            after: variables(&[("total", VariableValue::Integer(2.into()))]),
            grouping_keys: vec!["group".to_string()],
            default_before: true,
            default_after: false,
            row_signature: 44,
        },
    ];
    let envelope = query_evaluation_to_envelope(&contexts, query_metadata(7))
        .unwrap()
        .unwrap();

    assert_eq!(envelope.change_set().updated().len(), 2);
    assert_eq!(
        envelope.change_set().updated()[0].semantics(),
        UpdateSemantics::Replace
    );
    assert!(envelope.change_set().updated()[0].before().is_some());
    assert!(matches!(
        envelope.change_set().updated()[1].metadata(),
        UpdateMetadata::QueryAggregation {
            grouping_keys,
            default_before: true,
            default_after: false,
        } if grouping_keys.as_ref() == [Arc::<str>::from("group")]
    ));

    let result = query_result_from_envelope(&envelope).unwrap();
    assert_eq!(result.query_id, "query-a");
    assert_eq!(result.sequence, 7);
    assert_eq!(
        result.results,
        vec![
            ResultDiff::Add {
                data: json!({ "kind": "added" }),
                row_signature: 11,
            },
            ResultDiff::Update {
                data: json!({ "kind": "after" }),
                before: json!({ "kind": "before" }),
                after: json!({ "kind": "after" }),
                grouping_keys: None,
                row_signature: 22,
            },
            ResultDiff::Delete {
                data: json!({ "kind": "removed" }),
                row_signature: 33,
            },
            ResultDiff::Aggregation {
                before: Some(json!({ "total": 1 })),
                after: json!({ "total": 2 }),
                row_signature: 44,
            },
        ]
    );
    assert_eq!(result.metadata, *envelope.metadata());
    assert_eq!(result.profiling, envelope.system().profiling().cloned());
    assert_eq!(result.timestamp, envelope.system().timestamp());
}

#[test]
fn envelopes_share_immutable_changes_and_append_context_persistently() {
    let input = wrapper(SourceChange::Insert {
        element: element(ElementPropertyMap::from(json!({ "name": "Alice" })), 1_000),
    });
    let envelope = source_event_to_envelope(
        &input,
        SystemMetadataExtensions::default()
            .with_trace(TraceMetadata::new("trace-1", Some(Arc::from("span-1"))))
            .with_graph_epoch(5)
            .with_config_epoch(8),
    )
    .unwrap();
    let first = envelope.append_context(ContextContribution::new(
        ContextContributor::new(ContextContributorKind::Source, "source-a"),
        "partition",
        ContextValue::Unsigned(3),
    ));
    let second = first.append_context(ContextContribution::new(
        ContextContributor::new(ContextContributorKind::Query, "query-a"),
        "matched",
        ContextValue::Bool(true),
    ));

    assert!(Arc::ptr_eq(envelope.change_set(), first.change_set()));
    assert!(Arc::ptr_eq(first.change_set(), second.change_set()));
    assert!(Arc::ptr_eq(
        envelope.context().root(),
        second.context().root()
    ));
    assert!(Arc::ptr_eq(
        first.context().head().unwrap(),
        second.context().head().unwrap().parent().unwrap()
    ));
    assert_eq!(envelope.context().len(), 0);
    assert_eq!(first.context().len(), 1);
    assert_eq!(second.context().len(), 2);
    assert_ne!(envelope.id(), first.id());
    assert_ne!(first.id(), second.id());
    assert_eq!(envelope.system().graph_epoch(), Some(5));
    assert_eq!(envelope.system().config_epoch(), Some(8));
    assert_eq!(
        envelope.system().trace().unwrap(),
        &TraceMetadata::new("trace-1", Some(Arc::from("span-1")))
    );
}

#[test]
fn boundary_ids_are_deterministic_for_identical_batches() {
    let input = wrapper(SourceChange::Insert {
        element: element(ElementPropertyMap::from(json!({ "name": "Alice" })), 1_000),
    });
    let first = source_event_to_envelope(&input, SystemMetadataExtensions::default()).unwrap();
    let second = source_event_to_envelope(&input, SystemMetadataExtensions::default()).unwrap();

    assert_eq!(first.change_set().id(), second.change_set().id());
    assert_eq!(first.id(), second.id());
    assert_ne!(first.change_set().id().value(), first.id().value());
    assert_source_wrapper_eq(&source_event_from_envelope(&first).unwrap(), &input);

    let query_one = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[
                ("name", VariableValue::String("Alice".to_string())),
                ("age", VariableValue::Integer(42.into())),
            ]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let query_two = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[
                ("age", VariableValue::Integer(42.into())),
                ("name", VariableValue::String("Alice".to_string())),
            ]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    assert_eq!(query_one.change_set().id(), query_two.change_set().id());
    assert_eq!(query_one.id(), query_two.id());
}

#[test]
fn query_change_set_ids_include_typed_result_content() {
    let alice = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[("name", VariableValue::String("Alice".to_string()))]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let bob = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[("name", VariableValue::String("Bob".to_string()))]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();

    assert_ne!(alice.change_set().id(), bob.change_set().id());

    let typed_date = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[(
                "value",
                VariableValue::Date(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap()),
            )]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let legacy_equivalent_string = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[("value", VariableValue::String("2026-09-08".to_string()))]),
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    assert_ne!(
        typed_date.change_set().id(),
        legacy_equivalent_string.change_set().id(),
        "typed values that share a legacy JSON representation must remain distinct"
    );
}

#[test]
fn canonical_integer_encoding_distinguishes_sign_variants() {
    let positive = variables(&[("value", VariableValue::Integer(u64::MAX.into()))]);
    let negative = variables(&[("value", VariableValue::Integer((-1_i64).into()))]);

    assert_ne!(
        encode_query_variables(&positive).unwrap(),
        encode_query_variables(&negative).unwrap()
    );

    let positive_envelope = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: positive,
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let negative_envelope = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: negative,
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    assert_ne!(
        positive_envelope.change_set().id(),
        negative_envelope.change_set().id()
    );
}

#[test]
fn canonical_nested_values_ignore_map_insertion_order() {
    let mut first_object = BTreeMap::new();
    first_object.insert(
        "z".to_string(),
        VariableValue::List(vec![
            VariableValue::Integer(1.into()),
            VariableValue::Bool(true),
        ]),
    );
    first_object.insert("a".to_string(), VariableValue::String("first".to_string()));

    let mut second_object = BTreeMap::new();
    second_object.insert("a".to_string(), VariableValue::String("first".to_string()));
    second_object.insert(
        "z".to_string(),
        VariableValue::List(vec![
            VariableValue::Integer(1.into()),
            VariableValue::Bool(true),
        ]),
    );

    let first = variables(&[
        ("nested", VariableValue::Object(first_object)),
        ("tail", VariableValue::Null),
    ]);
    let second = variables(&[
        ("tail", VariableValue::Null),
        ("nested", VariableValue::Object(second_object)),
    ]);

    assert_eq!(
        encode_query_variables(&first).unwrap(),
        encode_query_variables(&second).unwrap()
    );
}

#[test]
fn canonical_integer_bytes_are_fixed_width_and_versioned() {
    let values = variables(&[
        ("positive", VariableValue::Integer(u64::MAX.into())),
        ("negative", VariableValue::Integer((-1_i64).into())),
    ]);
    let encoded = encode_query_variables(&values).unwrap();
    let encoded_hex = encoded
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    assert_eq!(
        encoded_hex,
        concat!(
            "44524153492d5156010000000000000002",
            "00000000000000086e6567617469766504ffffffffffffffff",
            "0000000000000008706f73697469766503ffffffffffffffff",
        )
    );
}

#[test]
fn canonical_float_encoding_preserves_nan_payloads_and_semantics() {
    let first_nan_bits = 0x7ff8_0000_0000_0001;
    let second_nan_bits = 0xfff8_0000_0000_0002;
    let first_nan = variables(&[(
        "value",
        VariableValue::Float(Float::from(f64::from_bits(first_nan_bits))),
    )]);
    let second_nan = variables(&[(
        "value",
        VariableValue::Float(Float::from(f64::from_bits(second_nan_bits))),
    )]);

    let first_bytes = encode_query_variables(&first_nan).unwrap();
    assert_eq!(first_bytes, encode_query_variables(&first_nan).unwrap());
    assert!(first_bytes.ends_with(&first_nan_bits.to_be_bytes()));
    assert!(encode_query_variables(&second_nan)
        .unwrap()
        .ends_with(&second_nan_bits.to_be_bytes()));
    assert_ne!(first_bytes, encode_query_variables(&second_nan).unwrap());

    let first_envelope = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: first_nan,
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let second_envelope = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: second_nan,
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    assert_ne!(
        first_envelope.change_set().id(),
        second_envelope.change_set().id()
    );

    let positive_zero = variables(&[("value", VariableValue::Float(Float::from(0.0)))]);
    let negative_zero = variables(&[("value", VariableValue::Float(Float::from(-0.0)))]);
    assert_eq!(
        encode_query_variables(&positive_zero).unwrap(),
        encode_query_variables(&negative_zero).unwrap(),
        "signed zero compares equal in Float and must have one canonical encoding"
    );
    let positive_zero_envelope = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: positive_zero,
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    let negative_zero_envelope = query_evaluation_to_envelope(
        &[QueryPartEvaluationContext::Adding {
            after: negative_zero,
            row_signature: 11,
        }],
        query_metadata(1),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        positive_zero_envelope.change_set().id(),
        negative_zero_envelope.change_set().id()
    );

    for value in [1.5, f64::INFINITY, f64::NEG_INFINITY] {
        let variables = variables(&[("value", VariableValue::Float(Float::from(value)))]);
        let encoded = encode_query_variables(&variables).unwrap();
        assert_eq!(encoded, encode_query_variables(&variables).unwrap());
        assert!(encoded.ends_with(&value.to_bits().to_be_bytes()));
    }
}

#[test]
fn canonical_element_float_encoding_matches_ordered_float_semantics() {
    fn graph_float(value: f64) -> SourceEventWrapper {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::Float(OrderedFloat(value)));
        wrapper(SourceChange::Insert {
            element: element(properties, 1_000),
        })
    }

    fn change(wrapper: &SourceEventWrapper) -> &SourceChange {
        let SourceEvent::Change(change) = &wrapper.event else {
            panic!("expected source change");
        };
        change
    }

    let first_nan = graph_float(f64::from_bits(0x7ff8_0000_0000_0001));
    let second_nan = graph_float(f64::from_bits(0xfff8_0000_0000_0002));
    assert_eq!(first_nan.event, second_nan.event);
    assert_eq!(
        encode_source_change(change(&first_nan)).unwrap(),
        encode_source_change(change(&second_nan)).unwrap()
    );
    assert_eq!(
        source_event_to_envelope(&first_nan, SystemMetadataExtensions::default())
            .unwrap()
            .change_set()
            .id(),
        source_event_to_envelope(&second_nan, SystemMetadataExtensions::default())
            .unwrap()
            .change_set()
            .id()
    );

    let positive_zero = graph_float(0.0);
    let negative_zero = graph_float(-0.0);
    assert_eq!(
        encode_source_change(change(&positive_zero)).unwrap(),
        encode_source_change(change(&negative_zero)).unwrap()
    );
    assert_eq!(
        source_event_to_envelope(&positive_zero, SystemMetadataExtensions::default())
            .unwrap()
            .change_set()
            .id(),
        source_event_to_envelope(&negative_zero, SystemMetadataExtensions::default())
            .unwrap()
            .change_set()
            .id()
    );

    let positive_infinity = graph_float(f64::INFINITY);
    let negative_infinity = graph_float(f64::NEG_INFINITY);
    let positive_infinity_bytes = encode_source_change(change(&positive_infinity)).unwrap();
    assert_eq!(
        positive_infinity_bytes,
        encode_source_change(change(&positive_infinity)).unwrap()
    );
    assert_ne!(
        positive_infinity_bytes,
        encode_source_change(change(&negative_infinity)).unwrap()
    );
}

#[test]
fn canonical_fixed_datetimes_follow_temporal_equality() {
    fn query_value_envelope(value: VariableValue) -> ChangeEnvelope {
        query_evaluation_to_envelope(
            &[QueryPartEvaluationContext::Adding {
                after: variables(&[("value", value)]),
                row_signature: 11,
            }],
            query_metadata(1),
        )
        .unwrap()
        .unwrap()
    }

    fn source_change(wrapper: &SourceEventWrapper) -> &SourceChange {
        let SourceEvent::Change(change) = &wrapper.event else {
            panic!("expected source change");
        };
        change
    }

    fn graph_datetime(value: chrono::DateTime<FixedOffset>) -> SourceEventWrapper {
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::ZonedDateTime(value));
        wrapper(SourceChange::Insert {
            element: element(properties, 1_000),
        })
    }

    let utc = FixedOffset::east_opt(0)
        .unwrap()
        .with_ymd_and_hms(2020, 1, 1, 0, 0, 0)
        .single()
        .unwrap();
    let plus_one = FixedOffset::east_opt(3_600)
        .unwrap()
        .with_ymd_and_hms(2020, 1, 1, 1, 0, 0)
        .single()
        .unwrap();
    assert_eq!(utc, plus_one);

    let utc_graph = graph_datetime(utc);
    let plus_one_graph = graph_datetime(plus_one);
    assert_eq!(
        encode_source_change(source_change(&utc_graph)).unwrap(),
        encode_source_change(source_change(&plus_one_graph)).unwrap()
    );
    assert_eq!(
        source_event_to_envelope(&utc_graph, SystemMetadataExtensions::default())
            .unwrap()
            .change_set()
            .id(),
        source_event_to_envelope(&plus_one_graph, SystemMetadataExtensions::default())
            .unwrap()
            .change_set()
            .id()
    );

    let utc_zoned =
        VariableValue::ZonedDateTime(VarZonedDateTime::new(utc, Some("Etc/UTC".to_string())));
    let plus_one_zoned =
        VariableValue::ZonedDateTime(VarZonedDateTime::new(plus_one, Some("Etc/UTC".to_string())));
    assert_eq!(utc_zoned, plus_one_zoned);
    assert_eq!(
        encode_query_variables(&variables(&[("value", utc_zoned.clone())])).unwrap(),
        encode_query_variables(&variables(&[("value", plus_one_zoned.clone())])).unwrap()
    );
    assert_eq!(
        query_value_envelope(utc_zoned).change_set().id(),
        query_value_envelope(plus_one_zoned).change_set().id()
    );

    let utc_named =
        VariableValue::ZonedDateTime(VarZonedDateTime::new(utc, Some("Etc/UTC".to_string())));
    let differently_named =
        VariableValue::ZonedDateTime(VarZonedDateTime::new(utc, Some("UTC".to_string())));
    assert_ne!(utc_named, differently_named);
    assert_ne!(
        encode_query_variables(&variables(&[("value", utc_named.clone())])).unwrap(),
        encode_query_variables(&variables(&[("value", differently_named.clone())])).unwrap()
    );
    assert_ne!(
        query_value_envelope(utc_named).change_set().id(),
        query_value_envelope(differently_named).change_set().id()
    );

    let local_time = NaiveTime::from_hms_nano_opt(12, 30, 45, 123_456_789).unwrap();
    let utc_time = VariableValue::ZonedTime(VarZonedTime::new(
        local_time,
        FixedOffset::east_opt(0).unwrap(),
    ));
    let plus_one_time = VariableValue::ZonedTime(VarZonedTime::new(
        local_time,
        FixedOffset::east_opt(3_600).unwrap(),
    ));
    assert_ne!(utc_time, plus_one_time);
    assert_ne!(
        encode_query_variables(&variables(&[("value", utc_time.clone())])).unwrap(),
        encode_query_variables(&variables(&[("value", plus_one_time.clone())])).unwrap()
    );
    assert_ne!(
        query_value_envelope(utc_time).change_set().id(),
        query_value_envelope(plus_one_time).change_set().id()
    );
}

#[test]
fn query_change_set_ids_include_available_epochs() {
    let contexts = [QueryPartEvaluationContext::Adding {
        after: variables(&[("name", VariableValue::String("Alice".to_string()))]),
        row_signature: 11,
    }];
    let baseline = query_evaluation_to_envelope(
        &contexts,
        query_metadata(1).with_extensions(
            SystemMetadataExtensions::default()
                .with_graph_epoch(5)
                .with_config_epoch(8),
        ),
    )
    .unwrap()
    .unwrap();
    let graph_changed = query_evaluation_to_envelope(
        &contexts,
        query_metadata(1).with_extensions(
            SystemMetadataExtensions::default()
                .with_graph_epoch(6)
                .with_config_epoch(8),
        ),
    )
    .unwrap()
    .unwrap();
    let config_changed = query_evaluation_to_envelope(
        &contexts,
        query_metadata(1).with_extensions(
            SystemMetadataExtensions::default()
                .with_graph_epoch(5)
                .with_config_epoch(9),
        ),
    )
    .unwrap()
    .unwrap();

    assert_ne!(baseline.change_set().id(), graph_changed.change_set().id());
    assert_ne!(baseline.change_set().id(), config_changed.change_set().id());
}

#[test]
fn all_noop_query_results_do_not_create_an_envelope() {
    assert!(
        query_evaluation_to_envelope(&[QueryPartEvaluationContext::Noop], query_metadata(1))
            .unwrap()
            .is_none()
    );
}
