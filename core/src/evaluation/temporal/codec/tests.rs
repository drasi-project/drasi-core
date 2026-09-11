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
    sync::Arc,
};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use drasi_query_ast::ast::{
    BinaryExpression, CaseExpression, Expression, FunctionExpression, IteratorExpression,
    ListExpression, Literal, ObjectExpression, UnaryExpression,
};
use ordered_float::OrderedFloat;

use super::*;
use crate::{
    evaluation::{
        temporal::{fixtures, *},
        variable_value::{
            duration::Duration, float::Float, integer::Integer, zoned_datetime::ZonedDateTime,
            zoned_time::ZonedTime, ListRange, RangeBound,
        },
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue},
};

fn roundtrip(value: &VariableValue) -> VariableValue {
    let encoded = encode_value(value).unwrap();
    let decoded = decode_value(&encoded).unwrap();
    assert_eq!(encode_value(&decoded).unwrap(), encoded);
    decoded
}

fn zoned() -> DateTime<FixedOffset> {
    DateTime::from_naive_utc_and_offset(
        NaiveDate::from_ymd_opt(2026, 9, 10)
            .unwrap()
            .and_hms_nano_opt(12, 13, 14, 987_654_321)
            .unwrap(),
        FixedOffset::east_opt(3661).unwrap(),
    )
}

fn metadata() -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("source:with:colons", "node"),
        labels: Arc::from([Arc::from("A"), Arc::from("B")]),
        effective_from: u64::MAX,
    }
}

fn properties() -> ElementPropertyMap {
    let values = BTreeMap::from([
        ("null".into(), ElementValue::Null),
        ("bool".into(), ElementValue::Bool(true)),
        ("float".into(), ElementValue::Float(OrderedFloat(-0.0))),
        ("integer".into(), ElementValue::Integer(i64::MIN)),
        ("string".into(), ElementValue::String(Arc::from("text"))),
        ("list".into(), ElementValue::List(vec![ElementValue::Null])),
        (
            "object".into(),
            ElementValue::Object(BTreeMap::<String, ElementValue>::new().into()),
        ),
        (
            "local".into(),
            ElementValue::LocalDateTime(zoned().naive_utc()),
        ),
        ("zoned".into(), ElementValue::ZonedDateTime(zoned())),
    ]);
    values.into()
}

#[test]
fn all_variable_variants_roundtrip_without_presentation_json() {
    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let time = NaiveTime::from_hms_nano_opt(23, 59, 59, 1_234_567_890).unwrap();
    let node = Element::Node {
        metadata: metadata(),
        properties: properties(),
    };
    let relation = Element::Relation {
        metadata: metadata(),
        properties: properties(),
        in_node: ElementReference::new("source", "in"),
        out_node: ElementReference::new("source", "out"),
    };
    let values = vec![
        VariableValue::Null,
        VariableValue::Bool(true),
        VariableValue::Float(Float::from(-0.0)),
        VariableValue::Integer(Integer::from(u64::MAX)),
        VariableValue::Integer(Integer::from(i64::MIN)),
        VariableValue::String("Awaiting".into()),
        VariableValue::List(vec![VariableValue::Null, VariableValue::Awaiting]),
        VariableValue::Object(BTreeMap::from([(
            "nested".into(),
            VariableValue::List(vec![VariableValue::ElementReference(
                ElementReference::new("s", "r"),
            )]),
        )])),
        VariableValue::Date(date),
        VariableValue::LocalTime(time),
        VariableValue::ZonedTime(ZonedTime::new(time, FixedOffset::west_opt(3661).unwrap())),
        VariableValue::LocalDateTime(date.and_time(time)),
        VariableValue::ZonedDateTime(ZonedDateTime::new(zoned(), Some("Europe/Paris".into()))),
        VariableValue::Duration(Duration::new(
            chrono::Duration::nanoseconds(-1_234_567_891),
            -2,
            17,
        )),
        VariableValue::Expression(FunctionExpression::function(
            "f".into(),
            vec![UnaryExpression::literal(Literal::Real(-0.0))],
            47,
        )),
        VariableValue::ListRange(ListRange {
            start: RangeBound::Unbounded,
            end: RangeBound::Index(-4),
        }),
        VariableValue::ListRange(ListRange {
            start: RangeBound::Index(i64::MIN),
            end: RangeBound::Unbounded,
        }),
        VariableValue::Element(Arc::new(node)),
        VariableValue::Element(Arc::new(relation)),
        VariableValue::ElementMetadata(metadata()),
        VariableValue::ElementReference(ElementReference::new("s", "r")),
        VariableValue::Awaiting,
    ];
    for value in values {
        let restored = roundtrip(&value);
        assert_eq!(restored, value);
    }
    let VariableValue::ZonedDateTime(restored) = roundtrip(&VariableValue::ZonedDateTime(
        ZonedDateTime::new(zoned(), Some("Europe/Paris".into())),
    )) else {
        panic!("lost zoned datetime type");
    };
    assert_eq!(restored.datetime().offset().local_minus_utc(), 3661);
    assert_eq!(restored.timezone_name().as_deref(), Some("Europe/Paris"));
}

#[test]
fn float_bits_and_unsigned_range_survive() {
    for bits in [
        0,
        1 << 63,
        1,
        u64::MAX,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_1234,
        0x7ff0_0000_0000_4321,
    ] {
        let value = VariableValue::Float(Float::from(f64::from_bits(bits)));
        let VariableValue::Float(restored) = roundtrip(&value) else {
            panic!("lost float type");
        };
        assert_eq!(restored.to_bits(), bits);
        let element = VariableValue::Element(Arc::new(Element::Node {
            metadata: metadata(),
            properties: BTreeMap::from([(
                "bits".into(),
                ElementValue::Float(OrderedFloat(f64::from_bits(bits))),
            )])
            .into(),
        }));
        let VariableValue::Element(restored) = roundtrip(&element) else {
            panic!("lost element type");
        };
        let ElementValue::Float(restored) = restored.get_property("bits") else {
            panic!("lost property type");
        };
        assert_eq!(restored.0.to_bits(), bits);
        let expression = VariableValue::Expression(UnaryExpression::literal(Literal::Real(
            f64::from_bits(bits),
        )));
        let VariableValue::Expression(Expression::UnaryExpression(UnaryExpression::Literal(
            Literal::Real(restored),
        ))) = roundtrip(&expression)
        else {
            panic!("lost real expression");
        };
        assert_eq!(restored.to_bits(), bits);
    }
    let value = roundtrip(&VariableValue::Integer(Integer::from(u64::MAX)));
    assert_eq!(value.as_u64(), Some(u64::MAX));
    assert_eq!(value.as_i64(), None);
}

#[test]
fn temporal_extremes_preserve_precision_and_components() {
    for value in [
        VariableValue::Date(NaiveDate::MIN),
        VariableValue::Date(NaiveDate::MAX),
        VariableValue::LocalDateTime(NaiveDateTime::MIN),
        VariableValue::LocalDateTime(NaiveDateTime::MAX),
        VariableValue::Duration(Duration::new(chrono::Duration::MIN, i64::MIN, i64::MAX)),
        VariableValue::Duration(Duration::new(chrono::Duration::MAX, i64::MAX, i64::MIN)),
    ] {
        assert_eq!(roundtrip(&value), value);
    }
}

#[test]
fn every_reachable_expression_shape_roundtrips() {
    let atom = UnaryExpression::ident("x");
    let literals = vec![
        Literal::Integer(i64::MIN),
        Literal::Real(-0.0),
        Literal::Boolean(false),
        Literal::Text("s".into()),
        Literal::Date("2026-01-01".into()),
        Literal::LocalTime("00:00:00.123456789".into()),
        Literal::ZonedTime("00:00:00Z".into()),
        Literal::LocalDateTime("2026-01-01T00:00:00".into()),
        Literal::ZonedDateTime("2026-01-01T00:00:00Z".into()),
        Literal::Duration("P1Y2M".into()),
        Literal::Object(vec![(
            "e".into(),
            Literal::Expression(Box::new(atom.clone())),
        )]),
        Literal::Expression(Box::new(atom.clone())),
        Literal::Null,
    ];
    let mut expressions: Vec<_> = literals.into_iter().map(UnaryExpression::literal).collect();
    let unary = vec![
        UnaryExpression::Not(Box::new(atom.clone())),
        UnaryExpression::Exists(Box::new(atom.clone())),
        UnaryExpression::IsNull(Box::new(atom.clone())),
        UnaryExpression::IsNotNull(Box::new(atom.clone())),
        UnaryExpression::Property {
            name: "x".into(),
            key: "p".into(),
        },
        UnaryExpression::ExpressionProperty {
            exp: Box::new(atom.clone()),
            key: "p".into(),
        },
        UnaryExpression::Parameter("p".into()),
        UnaryExpression::Identifier("x".into()),
        UnaryExpression::Variable {
            name: "v".into(),
            value: Box::new(atom.clone()),
        },
        UnaryExpression::Alias {
            source: Box::new(atom.clone()),
            alias: "alias".into(),
        },
        UnaryExpression::ListRange {
            start_bound: Some(Box::new(atom.clone())),
            end_bound: None,
        },
    ];
    expressions.extend(unary.into_iter().map(Expression::UnaryExpression));
    let binary: &[fn(Expression, Expression) -> Expression] = &[
        BinaryExpression::and,
        BinaryExpression::or,
        BinaryExpression::eq,
        BinaryExpression::ne,
        BinaryExpression::lt,
        BinaryExpression::le,
        BinaryExpression::gt,
        BinaryExpression::ge,
        BinaryExpression::in_,
        BinaryExpression::add,
        BinaryExpression::subtract,
        BinaryExpression::multiply,
        BinaryExpression::divide,
        BinaryExpression::modulo,
        BinaryExpression::exponent,
        BinaryExpression::has_label,
        BinaryExpression::index,
        BinaryExpression::starts_with,
        BinaryExpression::ends_with,
        BinaryExpression::contains,
    ];
    for constructor in binary {
        expressions.push(constructor(atom.clone(), atom.clone()));
    }
    expressions.extend([
        FunctionExpression::function("f".into(), vec![atom.clone()], 123),
        CaseExpression::case(
            Some(atom.clone()),
            vec![(atom.clone(), atom.clone())],
            Some(atom.clone()),
        ),
        ListExpression::list(vec![atom.clone()]),
        ObjectExpression::object_from_vec(vec![("value".into(), atom.clone())]),
        IteratorExpression::map_with_filter("i".into(), atom.clone(), atom.clone(), atom),
    ]);
    for expression in expressions {
        let value = VariableValue::Expression(expression);
        assert_eq!(roundtrip(&value), value);
    }
}

#[test]
fn records_and_tickets_use_the_same_versioned_codec() {
    let mut row = fixtures::input();
    let ticket = fixtures::ticket(&mut row);
    row.tickets.insert(ticket.id.clone(), ticket.clone());
    row.applied.last_delivered = Presence::Default(DeliveredRow {
        variables: row.variables.clone(),
        row_signature: row.row_signature,
    });
    row.applied.predicate.push(AggregateContribution {
        call: fixtures::call(),
        owner: ContributionOwner::PartDefault(1),
        key: ContributionKey::GroupBy(vec![VariableValue::Awaiting]),
        arguments: vec![VariableValue::Integer(Integer::from(u64::MAX))],
        context: row.context.clone(),
    });
    row.history.insert(
        fixtures::call(),
        HistoryCapture {
            revision: row.source_revision,
            value: VariableValue::Awaiting,
        },
    );
    row.values.insert(
        fixtures::call(),
        FunctionValue {
            value: VariableValue::Float(Float::from(f64::from_bits(0x7ff8_0000_0000_1234))),
        },
    );
    let group = GroupState {
        id: TemporalGroupId {
            namespace: row.id.namespace,
            producer: PartId(1),
            incarnation: Incarnation(4),
        },
        grouping: GroupingKey(vec![VariableValue::Null]),
        members: BTreeSet::from([row.id]),
        default_row: false,
        cells: BTreeSet::new(),
        last_delivered: Presence::Real(DeliveredRow {
            variables: row.variables.clone(),
            row_signature: row.row_signature,
        }),
    };
    let records = [
        TemporalRecord::Epoch(EpochState::new(row.id.namespace)),
        TemporalRecord::Part(PartState {
            namespace: row.id.namespace,
            part: row.id.part,
            inputs: BTreeSet::from([row.id]),
        }),
        TemporalRecord::Origin {
            key: row.origin_key(),
            input: row.id,
        },
        TemporalRecord::Input(row.clone()),
        TemporalRecord::GroupOrigin {
            key: group.origin(),
            group: group.id,
        },
        TemporalRecord::Group(group.clone()),
        TemporalRecord::Cell(FunctionCell {
            id: FunctionCellId {
                owner: StateOwner::Group(group.id),
                call: fixtures::call(),
            },
            state: FunctionState::TrueFor(TrueForState::default()),
            subscribers: BTreeSet::from([row.id]),
        }),
    ];
    for record in records {
        let bytes = encode_record(&record).unwrap();
        let restored = decode_record(&record.key(), &bytes).unwrap();
        assert_eq!(encode_record(&restored).unwrap(), bytes);
    }
    let bytes = encode_ticket(&ticket).unwrap();
    let restored = decode_ticket(&bytes).unwrap();
    assert_eq!(restored.id, ticket.id);
    assert!(row.accepts(&restored));
}

#[test]
fn corrupt_unsupported_and_legacy_data_are_explicit_errors() {
    let value = encode_value(&VariableValue::Null).unwrap();
    for length in 0..value.len() {
        assert!(decode_value(&value[..length]).is_err());
    }
    assert!(matches!(
        decode_value(b"legacy temporal"),
        Err(TemporalCodecError::MigrationRequired)
    ));
    let mut future = value.clone();
    future[4..6].copy_from_slice(&2_u16.to_be_bytes());
    assert!(matches!(
        decode_value(&future),
        Err(TemporalCodecError::UnsupportedVersion { found: 2 })
    ));
    let mut unknown = value.clone();
    unknown[6] = 255;
    assert!(matches!(
        decode_value(&unknown),
        Err(TemporalCodecError::UnsupportedRecord { kind: 255 })
    ));
    let mut trailing = value;
    trailing.push(0);
    assert!(decode_value(&trailing).is_err());
    let invalid = encode(VALUE_KIND, &StoredValue::Negative(3)).unwrap();
    assert!(matches!(
        decode_value(&invalid),
        Err(TemporalCodecError::Corrupt(_))
    ));
    let invalid = encode(VALUE_KIND, &StoredValue::Date(i32::MAX)).unwrap();
    assert!(decode_value(&invalid).is_err());
    let invalid = encode(
        VALUE_KIND,
        &StoredValue::ZonedTime {
            time: NaiveTime::from_hms_opt(0, 0, 0).unwrap().into(),
            offset: 86_400,
        },
    )
    .unwrap();
    assert!(decode_value(&invalid).is_err());
    let invalid = encode(
        VALUE_KIND,
        &StoredValue::Duration {
            seconds: 1,
            nanos: -1,
            years: 0,
            months: 0,
        },
    )
    .unwrap();
    assert!(decode_value(&invalid).is_err());
}

#[test]
fn wrong_epoch_plan_part_or_record_key_is_not_a_cache_miss() {
    let row = fixtures::input();
    let bytes = encode_record(&TemporalRecord::Input(row.clone())).unwrap();
    for id in [
        TemporalInputId {
            incarnation: Incarnation(99),
            ..row.id
        },
        TemporalInputId {
            part: PartId(99),
            ..row.id
        },
        TemporalInputId {
            namespace: TemporalNamespace {
                epoch: QueryEpoch([8; 16]),
                ..row.id.namespace
            },
            ..row.id
        },
        TemporalInputId {
            namespace: TemporalNamespace {
                plan: PlanVersion(99),
                ..row.id.namespace
            },
            ..row.id
        },
    ] {
        assert!(matches!(
            decode_record(&TemporalKey::Input(id), &bytes),
            Err(TemporalCodecError::KeyMismatch | TemporalCodecError::PlanVersionMismatch { .. })
        ));
    }
}

#[test]
fn group_identity_ignores_element_updates_but_records_remain_lossless() {
    let key = |value| {
        TemporalKey::GroupOrigin(GroupOrigin {
            namespace: fixtures::namespace(),
            producer: PartId(1),
            grouping: GroupingKey(vec![value]),
        })
    };
    let node = Element::Node {
        metadata: metadata(),
        properties: properties(),
    };
    let mut updated = node.clone();
    updated.update_effective_time(200);
    let before = key(VariableValue::Element(Arc::new(node)));
    let after = key(VariableValue::Element(Arc::new(updated)));
    assert_eq!(encode_key(&before).unwrap(), encode_key(&after).unwrap());
    assert_ne!(
        encode_key(&before).unwrap(),
        encode_key(&key(VariableValue::ElementReference(metadata().reference))).unwrap()
    );
    assert_eq!(
        encode_key(&key(VariableValue::Float(Float::from(-0.0)))).unwrap(),
        encode_key(&key(VariableValue::Float(Float::from(0.0)))).unwrap()
    );
    let ns = fixtures::namespace();
    let TemporalKey::GroupOrigin(before) = before else {
        unreachable!()
    };
    let TemporalKey::GroupOrigin(after) = after else {
        unreachable!()
    };
    let group = TemporalGroupId {
        namespace: ns,
        producer: PartId(1),
        incarnation: Incarnation(3),
    };
    let before_record = TemporalRecord::GroupOrigin { key: before, group };
    let after_record = TemporalRecord::GroupOrigin { key: after, group };
    assert_ne!(
        encode_record(&before_record).unwrap(),
        encode_record(&after_record).unwrap()
    );
}

#[test]
fn an_epoch_manifest_remains_discoverable_across_plan_versions() {
    let old = fixtures::namespace();
    let new = TemporalNamespace {
        plan: PlanVersion(2),
        ..old
    };
    assert_eq!(
        encode_key(&TemporalKey::Epoch(old)).unwrap(),
        encode_key(&TemporalKey::Epoch(new)).unwrap()
    );
    assert!(encode_key(&TemporalKey::Epoch(old))
        .unwrap()
        .ends_with(b":manifest"));
    let bytes = encode_record(&TemporalRecord::Epoch(EpochState::new(old))).unwrap();
    assert!(matches!(
        decode_record(&TemporalKey::Epoch(new), &bytes),
        Err(TemporalCodecError::PlanVersionMismatch {
            expected: 2,
            found: 1
        })
    ));
}

#[test]
fn catalog_preserves_raw_query_namespace_and_revision() {
    let catalog = TemporalCatalog {
        namespace: fixtures::namespace(),
        query: "MATCH (n:Sensor)\nWHERE n.label = '測定'\nRETURN n  ".into(),
        next_revision: SourceRevision(u64::MAX),
    };
    let bytes = encode_catalog(&catalog).unwrap();
    assert_eq!(decode_catalog(&bytes).unwrap(), catalog);
    assert_eq!(bytes[6], CATALOG_KIND);
    assert!(CATALOG_KEY.starts_with(KEY_PREFIX));
    assert!(!CATALOG_KEY.starts_with(&epoch_prefix(catalog.namespace.epoch)));
}

#[test]
fn catalog_rejects_corruption_versions_and_other_record_kinds() {
    let catalog = TemporalCatalog {
        namespace: fixtures::namespace(),
        query: "RETURN 1".into(),
        next_revision: SourceRevision(3),
    };
    let bytes = encode_catalog(&catalog).unwrap();
    for end in 0..bytes.len() {
        assert!(decode_catalog(&bytes[..end]).is_err());
    }
    assert!(matches!(
        decode_catalog(b"legacy temporal"),
        Err(TemporalCodecError::MigrationRequired)
    ));
    let mut unsupported = bytes.clone();
    unsupported[4..6].copy_from_slice(&(CODEC_VERSION + 1).to_be_bytes());
    assert!(matches!(
        decode_catalog(&unsupported),
        Err(TemporalCodecError::UnsupportedVersion { .. })
    ));
    let mut unknown = bytes.clone();
    unknown[6] = 255;
    assert!(matches!(
        decode_catalog(&unknown),
        Err(TemporalCodecError::UnsupportedRecord { kind: 255 })
    ));
    let mut trailing = bytes;
    trailing.push(0);
    assert!(matches!(
        decode_catalog(&trailing),
        Err(TemporalCodecError::Corrupt(_))
    ));
    assert!(decode_catalog(&encode_value(&VariableValue::Null).unwrap()).is_err());
    let record = TemporalRecord::Epoch(EpochState::new(catalog.namespace));
    assert!(decode_catalog(&encode_record(&record).unwrap()).is_err());
    assert!(decode_record(&record.key(), &encode_catalog(&catalog).unwrap()).is_err());
}

#[test]
fn part_records_reject_cross_namespace_membership_and_wrong_storage_keys() {
    let input = fixtures::input().id;
    let part = PartState {
        namespace: input.namespace,
        part: input.part,
        inputs: BTreeSet::from([input]),
    };
    let record = TemporalRecord::Part(part.clone());
    let bytes = encode_record(&record).unwrap();
    let wrong_part = TemporalKey::Part {
        namespace: part.namespace,
        part: PartId(part.part.0 + 1),
    };
    assert!(matches!(
        decode_record(&wrong_part, &bytes),
        Err(TemporalCodecError::KeyMismatch)
    ));
    let wrong_plan = TemporalKey::Part {
        namespace: TemporalNamespace {
            plan: PlanVersion(99),
            ..part.namespace
        },
        part: part.part,
    };
    assert!(matches!(
        decode_record(&wrong_plan, &bytes),
        Err(TemporalCodecError::PlanVersionMismatch { .. })
    ));
    let invalid = TemporalRecord::Part(PartState {
        inputs: BTreeSet::from([TemporalInputId {
            namespace: TemporalNamespace {
                epoch: QueryEpoch([99; 16]),
                ..part.namespace
            },
            ..input
        }]),
        ..part
    });
    assert!(encode_record(&invalid).is_err());
    let corrupt = encode(record_kind(&invalid), &invalid).unwrap();
    assert!(matches!(
        decode_record(&invalid.key(), &corrupt),
        Err(TemporalCodecError::State(
            TemporalStateError::NamespaceMismatch
        ))
    ));
}

#[test]
fn history_and_window_cells_preserve_executable_values() {
    let row = fixtures::input();
    let states = [
        FunctionState::Aggregate,
        FunctionState::UninitializedHistory,
        FunctionState::History(HistoryState {
            revision: row.source_revision,
            current: VariableValue::Integer(Integer::from(u64::MAX)),
            previous: VariableValue::List(vec![
                VariableValue::Float(Float::from(f64::from_bits(0x7ff8_0000_0000_1234))),
                VariableValue::Duration(Duration::new(
                    chrono::Duration::nanoseconds(1_234_567_891),
                    2,
                    17,
                )),
                VariableValue::Awaiting,
            ]),
        }),
        FunctionState::SlidingWindow(SlidingWindowState {
            samples: BTreeMap::from([(
                Generation(0),
                WindowSample {
                    revision: row.source_revision,
                    expires_at: 321,
                    value: VariableValue::Expression(UnaryExpression::ident("saved")),
                },
            )]),
            next_sample: Generation(1),
        }),
    ];
    for state in states {
        let record = TemporalRecord::Cell(FunctionCell {
            id: FunctionCellId {
                owner: StateOwner::Input(row.id),
                call: fixtures::call(),
            },
            state,
            subscribers: BTreeSet::from([row.id]),
        });
        let bytes = encode_record(&record).unwrap();
        let restored = decode_record(&record.key(), &bytes).unwrap();
        assert_eq!(encode_record(&restored).unwrap(), bytes);
    }
}
