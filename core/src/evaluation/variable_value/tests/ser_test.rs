// Copyright 2024 The Drasi Authors.
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

use crate::{
    evaluation::variable_value::{
        duration::Duration, zoned_datetime::ZonedDateTime, zoned_time::ZonedTime, ListRange,
        RangeBound, VariableValue,
    },
    models::{Element, ElementMetadata, ElementReference},
};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime};
use drasi_query_ast::ast::UnaryExpression;
use serde_json::json;
use std::{collections::BTreeMap, io, sync::Arc};

#[test]
fn test_serializing_integer() {
    let value = VariableValue::Integer(42.into());
    let ser_value = serde_json::to_string(&value).unwrap();
    assert_eq!(ser_value, json!(42).to_string());
}

#[test]
fn test_serializing_float() {
    let value = VariableValue::Float(42.0.into());
    let ser_value = serde_json::to_string(&value).unwrap();
    assert_eq!(ser_value, json!(42.0).to_string());
}

#[test]
fn test_serializing_string() {
    let value = VariableValue::String("drasi".into());
    let ser_value = serde_json::to_value(&value).unwrap();
    assert_eq!(ser_value, json!("drasi"));
}

#[test]
fn test_serializing_list() {
    let vec = vec![
        VariableValue::Integer(42.into()),
        VariableValue::Integer(43.into()),
        VariableValue::String("Drasi loves rust".to_string()),
    ];
    let alist = VariableValue::List(vec);
    let ser_value = serde_json::to_value(&alist).unwrap();
    assert_eq!(ser_value, json!([42, 43, "Drasi loves rust"]));
}

#[test]
fn test_serializing_object() {
    let tree_map = {
        let mut tree_map = BTreeMap::new();
        tree_map.insert(
            "name".to_string(),
            VariableValue::String("Room 01_01_01".into()),
        );
        tree_map.insert(
            "comfortLevel".to_string(),
            VariableValue::Integer(50.into()),
        );
        tree_map.insert("temp".to_string(), VariableValue::Integer(74.into()));
        tree_map.insert("humidity".to_string(), VariableValue::Integer(44.into()));
        tree_map.insert("co2".to_string(), VariableValue::Integer(500.into()));
        tree_map
    };
    let obj = VariableValue::Object(tree_map);
    let ser_value = serde_json::to_value(&obj).unwrap();

    let expected = {
        let mut tree_map = BTreeMap::new();
        tree_map.insert("name".to_string(), json!("Room 01_01_01"));
        tree_map.insert("comfortLevel".to_string(), json!(50));
        tree_map.insert("temp".to_string(), json!(74));
        tree_map.insert("humidity".to_string(), json!(44));
        tree_map.insert("co2".to_string(), json!(500));
        tree_map
    };

    assert_eq!(ser_value, json!(expected));
}

fn object_with_value(value: VariableValue) -> VariableValue {
    VariableValue::Object(BTreeMap::from([
        ("a_before".to_owned(), VariableValue::Bool(true)),
        ("b_value".to_owned(), value),
        ("c_after".to_owned(), VariableValue::Bool(false)),
    ]))
}

fn assert_serialization_error(value: &VariableValue, expected: &str) {
    assert_eq!(
        serde_json::to_value(value)
            .expect_err("JSON value serialization must fail")
            .to_string(),
        expected
    );
    assert_eq!(
        serde_json::to_string(value)
            .expect_err("JSON string serialization must fail")
            .to_string(),
        expected
    );
    assert_eq!(
        rmp_serde::to_vec(value)
            .expect_err("MessagePack serialization must fail")
            .to_string(),
        expected
    );
    assert_eq!(
        rmp_serde::to_vec_named(value)
            .expect_err("named MessagePack serialization must fail")
            .to_string(),
        expected
    );
}

fn assert_nested_serialization_error(value: VariableValue, expected: &str) {
    let list = VariableValue::List(vec![
        VariableValue::Null,
        value.clone(),
        VariableValue::Null,
    ]);
    let object = object_with_value(value.clone());
    for value in [
        value,
        list.clone(),
        object.clone(),
        VariableValue::List(vec![object.clone()]),
        object_with_value(list),
        object_with_value(object),
    ] {
        assert_serialization_error(&value, expected);
    }
}

#[test]
fn test_serializing_awaiting_returns_error() {
    let outcome = std::panic::catch_unwind(|| serde_json::to_value(VariableValue::Awaiting));
    assert!(outcome.is_ok(), "unsupported values must not panic");
    assert_eq!(
        outcome.unwrap().unwrap_err().to_string(),
        "Cannot serialize VariableValue::Awaiting"
    );
    assert_nested_serialization_error(
        VariableValue::Awaiting,
        "Cannot serialize VariableValue::Awaiting",
    );
}

#[test]
fn test_serializing_unsupported_variants_returns_error() {
    let metadata = ElementMetadata {
        reference: ElementReference::new("private-source", "private-element"),
        labels: vec!["private-label".into()].into(),
        effective_from: 42,
    };
    let cases = [
        (
            "ZonedTime",
            VariableValue::ZonedTime(ZonedTime::new(
                NaiveTime::from_hms_opt(12, 34, 56).unwrap(),
                FixedOffset::east_opt(3600).unwrap(),
            )),
        ),
        (
            "Duration",
            VariableValue::Duration(Duration::new(chrono::Duration::seconds(42), 1, 2)),
        ),
        (
            "Expression",
            VariableValue::Expression(UnaryExpression::ident("private-expression")),
        ),
        (
            "ListRange",
            VariableValue::ListRange(ListRange {
                start: RangeBound::Index(1),
                end: RangeBound::Unbounded,
            }),
        ),
        (
            "Element",
            VariableValue::Element(Arc::new(Element::Node {
                metadata: metadata.clone(),
                properties: Default::default(),
            })),
        ),
        (
            "ElementMetadata",
            VariableValue::ElementMetadata(metadata.clone()),
        ),
        (
            "ElementReference",
            VariableValue::ElementReference(metadata.reference),
        ),
    ];
    for (variant, value) in cases {
        assert_nested_serialization_error(
            value,
            &format!("Cannot serialize VariableValue::{variant}"),
        );
    }
}

#[test]
fn test_serializing_integer_overflow_returns_error() {
    assert_serialization_error(&VariableValue::Integer(u64::MAX.into()), "Integer overflow");
}

#[test]
fn test_serializing_object_with_only_failing_field_returns_error() {
    let value = VariableValue::Object(BTreeMap::from([(
        "value".to_owned(),
        VariableValue::Integer(u64::MAX.into()),
    )]));
    assert_serialization_error(&value, "Integer overflow");
}

#[test]
fn test_serializing_object_with_surrounding_fields_returns_error() {
    let value = object_with_value(VariableValue::Integer(u64::MAX.into()));
    assert_serialization_error(&value, "Integer overflow");
}

#[test]
fn test_serializing_object_with_failing_list_returns_error() {
    let value = object_with_value(VariableValue::List(vec![
        VariableValue::Null,
        VariableValue::Integer(u64::MAX.into()),
        VariableValue::Null,
    ]));
    assert_serialization_error(&value, "Integer overflow");
}

#[test]
fn test_serializing_list_with_failing_object_returns_error() {
    let value = VariableValue::List(vec![
        VariableValue::Null,
        object_with_value(VariableValue::Integer(u64::MAX.into())),
        VariableValue::Null,
    ]);
    assert_serialization_error(&value, "Integer overflow");
}

#[test]
fn test_serializing_object_with_failing_object_returns_error() {
    let value = object_with_value(object_with_value(VariableValue::Integer(u64::MAX.into())));
    assert_serialization_error(&value, "Integer overflow");
}

#[test]
fn test_serializing_non_finite_float_returns_error() {
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_nested_serialization_error(VariableValue::Float(value.into()), "Float overflow");
    }
}

#[test]
fn test_serializing_object_propagates_serializer_entry_error() {
    #[derive(Default)]
    struct FailOnceWriter {
        bytes: Vec<u8>,
        failed: bool,
    }

    impl io::Write for FailOnceWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            // Fail during serialize_entry, but allow later writes (including map.end).
            if bytes == b"b_value" && !self.failed {
                self.failed = true;
                return Err(io::Error::other("injected map entry failure"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut writer = FailOnceWriter::default();
    let result = serde_json::to_writer(
        &mut writer,
        &object_with_value(VariableValue::Integer(42.into())),
    );
    assert!(
        result.is_err(),
        "expected entry error, got {result:?} with output {:?}",
        String::from_utf8_lossy(&writer.bytes)
    );
    let error = result.unwrap_err();
    assert!(error.is_io());
    assert_eq!(error.to_string(), "injected map entry failure");
    assert!(writer.failed);
    assert_eq!(writer.bytes, b"{\"a_before\":true,\"");
}

#[test]
fn test_serializing_supported_representations() {
    let date = NaiveDate::from_ymd_opt(2026, 9, 12).unwrap();
    let time = NaiveTime::from_hms_milli_opt(12, 34, 56, 123).unwrap();
    let datetime = DateTime::parse_from_rfc3339("2026-09-12T12:34:56.123+05:30").unwrap();
    let cases = [
        (VariableValue::Null, json!(null)),
        (VariableValue::Bool(false), json!(false)),
        (VariableValue::Bool(true), json!(true)),
        (VariableValue::Integer(0.into()), json!(0)),
        (VariableValue::Integer(i64::MIN.into()), json!(i64::MIN)),
        (VariableValue::Integer(i64::MAX.into()), json!(i64::MAX)),
        (VariableValue::Float(42.5.into()), json!(42.5)),
        (VariableValue::Float((-0.0).into()), json!(-0.0)),
        (
            VariableValue::String("Drasi \"query\"\n".into()),
            json!("Drasi \"query\"\n"),
        ),
        (VariableValue::Date(date), json!("2026-09-12")),
        (VariableValue::LocalTime(time), json!("12:34:56.123")),
        (
            VariableValue::LocalDateTime(date.and_time(time)),
            json!("2026-09-12T12:34:56.123"),
        ),
        (
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, Some("Asia/Kolkata".into()))),
            json!({
                "datetime": "2026-09-12T12:34:56.123+05:30",
                "timezone_name": "Asia/Kolkata",
            }),
        ),
        (
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, None)),
            json!({
                "datetime": "2026-09-12T12:34:56.123+05:30",
                "timezone_name": null,
            }),
        ),
        (VariableValue::List(vec![]), json!([])),
        (VariableValue::Object(BTreeMap::new()), json!({})),
        (
            VariableValue::List(vec![object_with_value(VariableValue::List(vec![
                VariableValue::Integer(42.into()),
                VariableValue::Null,
            ]))]),
            json!([{"a_before": true, "b_value": [42, null], "c_after": false}]),
        ),
    ];
    for (value, expected) in cases {
        assert_eq!(serde_json::to_value(&value).unwrap(), expected);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
        assert_eq!(
            rmp_serde::to_vec_named(&value).unwrap(),
            rmp_serde::to_vec_named(&expected).unwrap()
        );
        let compact_expected = match &value {
            VariableValue::ZonedDateTime(_) => {
                json!([expected["datetime"], expected["timezone_name"]])
            }
            _ => expected,
        };
        assert_eq!(
            rmp_serde::to_vec(&value).unwrap(),
            rmp_serde::to_vec(&compact_expected).unwrap()
        );
    }
}
