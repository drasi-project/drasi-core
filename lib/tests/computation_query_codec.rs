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
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    sync::Arc,
};

use chrono::{NaiveDate, NaiveTime, TimeZone};
use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        variable_value::{
            duration::Duration, float::Float, integer::Integer, zoned_datetime::ZonedDateTime,
            zoned_time::ZonedTime, ListRange, RangeBound, VariableValue,
        },
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
};
use drasi_lib::{channels::ResultDiff, computation::v1::*};
use drasi_query_ast::ast::{self, BinaryExpression, Literal, UnaryExpression};

fn variables(value: VariableValue) -> QueryVariables {
    BTreeMap::from([(Box::<str>::from("value"), value)])
}

#[test]
fn query_rows_retain_all_value_kinds_instead_of_using_lossy_legacy_json() {
    let offset = chrono::FixedOffset::east_opt(7200).expect("offset");
    let datetime = offset
        .timestamp_opt(1_700_000_000, 123456789)
        .single()
        .expect("datetime");
    let metadata = ElementMetadata {
        reference: ElementReference::new("source", "node"),
        labels: Arc::from([Arc::from("Item")]),
        effective_from: 123,
    };
    let expression = ast::CaseExpression::case(
        None,
        vec![(
            BinaryExpression::eq(
                UnaryExpression::literal(Literal::Integer(1)),
                UnaryExpression::literal(Literal::Integer(1)),
            ),
            ast::IteratorExpression::map_with_filter(
                Arc::from("x"),
                ast::ListExpression::list(vec![UnaryExpression::literal(Literal::Real(-0.0))]),
                ast::FunctionExpression::function(
                    Arc::from("f"),
                    vec![UnaryExpression::ident("x")],
                    7,
                ),
                UnaryExpression::is_not_null(UnaryExpression::ident("x")),
            ),
        )],
        Some(ast::ObjectExpression::object_from_vec(vec![(
            Arc::from("fallback"),
            UnaryExpression::literal(Literal::Text(Arc::from("value"))),
        )])),
    );
    let values = vec![
        VariableValue::Null,
        VariableValue::Bool(true),
        VariableValue::Float(Float::from(-0.0)),
        VariableValue::Float(Float::from(f64::INFINITY)),
        VariableValue::Float(Float::from(f64::from_bits(0x7ff8000000001234))),
        VariableValue::Integer(Integer::from(u64::MAX)),
        VariableValue::Integer(Integer::from(i64::MIN)),
        VariableValue::String("value".into()),
        VariableValue::List(vec![VariableValue::from(3)]),
        VariableValue::Object(BTreeMap::from([("key".into(), VariableValue::from(4))])),
        VariableValue::Date(NaiveDate::from_ymd_opt(2026, 9, 9).expect("date")),
        VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(1, 2, 3, 456).expect("time")),
        VariableValue::ZonedTime(ZonedTime::new(datetime.time(), offset)),
        VariableValue::LocalDateTime(datetime.naive_local()),
        VariableValue::ZonedDateTime(ZonedDateTime::new(
            datetime,
            Some("Example/Preserved-Name".into()),
        )),
        VariableValue::Duration(Duration::new(
            chrono::Duration::seconds(-3) + chrono::Duration::nanoseconds(-7),
            2,
            -1,
        )),
        VariableValue::ListRange(ListRange {
            start: RangeBound::Unbounded,
            end: RangeBound::Index(-1),
        }),
        VariableValue::Element(Arc::new(Element::Node {
            metadata: metadata.clone(),
            properties: ElementPropertyMap::new(),
        })),
        VariableValue::ElementMetadata(metadata.clone()),
        VariableValue::ElementReference(metadata.reference),
        VariableValue::Expression(expression),
        VariableValue::Awaiting,
    ];
    for value in values {
        let row = QueryChangeCodec::encode_row(
            "query",
            17,
            &variables(value),
            QueryRowKind::Row,
            RecordImage::Full,
        )
        .expect("encode typed value");
        let decoded = QueryChangeCodec::decode_row(&row).expect("decode typed value");
        let reencoded = QueryChangeCodec::encode_row(
            &decoded.query_id,
            decoded.signature,
            &decoded.values,
            decoded.kind,
            RecordImage::Full,
        )
        .expect("re-encode typed value");
        assert_eq!(
            row.payload(),
            reencoded.payload(),
            "exact type/bit/offset preservation"
        );
    }
}

#[test]
fn query_envelopes_preserve_order_aggregation_flags_and_legacy_projection() {
    let results = vec![
        QueryPartEvaluationContext::Adding {
            after: variables(VariableValue::from(1)),
            row_signature: 10,
        },
        QueryPartEvaluationContext::Noop,
        QueryPartEvaluationContext::Updating {
            before: variables(VariableValue::from(1)),
            after: variables(VariableValue::from(2)),
            row_signature: 10,
        },
        QueryPartEvaluationContext::Removing {
            before: variables(VariableValue::from(3)),
            row_signature: 11,
        },
        QueryPartEvaluationContext::Aggregation {
            before: None,
            after: variables(VariableValue::from(4)),
            grouping_keys: vec!["value".into()],
            default_before: true,
            default_after: false,
            row_signature: 12,
        },
    ];
    let metadata = QueryOutputMetadata {
        query_id: "query".into(),
        source_id: Some("source".into()),
        timestamp: chrono::Utc::now(),
        metadata: HashMap::new(),
        profiling: None,
    };
    let envelope = QueryChangeCodec::encode_evaluation(
        None,
        &ComponentId::try_new("query").expect("id"),
        SystemMetadata::new(StreamId::try_new("query/out").expect("stream"), 7),
        &results,
        metadata,
    )
    .expect("encode")
    .expect("nonempty");
    assert_eq!(
        envelope
            .changes()
            .operations()
            .iter()
            .map(ChangeOperation::ordinal)
            .collect::<Vec<_>>(),
        [0, 2, 3, 4]
    );
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024).expect("limit"));
    codec
        .register_schema(QueryChangeCodec::schema())
        .expect("register rows");
    let envelope = codec
        .decode(&codec.encode(&envelope).expect("encode frame"))
        .expect("decode frame");
    let decoded = QueryChangeCodec::decode_evaluation(&envelope).expect("typed results");
    assert_eq!(decoded.len(), 4);
    let QueryPartEvaluationContext::Aggregation {
        default_before,
        default_after,
        grouping_keys,
        ..
    } = &decoded[3]
    else {
        panic!("aggregation");
    };
    assert!(*default_before);
    assert!(!default_after);
    assert_eq!(grouping_keys, &["value"]);
    let legacy = QueryChangeCodec::to_legacy_result(&envelope).expect("legacy projection");
    assert_eq!(legacy.query_id, "query");
    assert_eq!(legacy.sequence, 7);
    assert!(matches!(
        legacy.results.as_slice(),
        [
            ResultDiff::Add { .. },
            ResultDiff::Update { .. },
            ResultDiff::Delete { .. },
            ResultDiff::Aggregation { .. }
        ]
    ));
}

#[test]
fn row_identity_is_scoped_by_query_and_metadata_cannot_relabel_a_foreign_set() {
    let values = variables(VariableValue::from(1));
    let first =
        QueryChangeCodec::encode_row("first", 1, &values, QueryRowKind::Row, RecordImage::Full)
            .expect("first");
    let second =
        QueryChangeCodec::encode_row("second", 1, &values, QueryRowKind::Row, RecordImage::Full)
            .expect("second");
    assert_ne!(first.identity(), second.identity());
    let mut envelope = QueryChangeCodec::encode_evaluation(
        None,
        &ComponentId::try_new("first").expect("id"),
        SystemMetadata::new(StreamId::try_new("first/out").expect("stream"), 1),
        &[QueryPartEvaluationContext::Adding {
            after: values,
            row_signature: 1,
        }],
        QueryOutputMetadata {
            query_id: "first".into(),
            source_id: None,
            timestamp: chrono::Utc::now(),
            metadata: HashMap::new(),
            profiling: None,
        },
    )
    .expect("encode")
    .expect("event");
    envelope
        .append_annotation(
            ContextEntry::try_new(
                ComponentId::try_new("other").expect("id"),
                "drasi.query-output.v1",
                ContextValue::Bytes(Arc::from(
                    serde_json::to_vec(&QueryOutputMetadata {
                        query_id: "second".into(),
                        source_id: None,
                        timestamp: chrono::Utc::now(),
                        metadata: HashMap::new(),
                        profiling: None,
                    })
                    .expect("metadata"),
                )),
            )
            .expect("entry"),
        )
        .expect("append");
    assert!(matches!(
        QueryChangeCodec::to_legacy_result(&envelope),
        Err(QueryCodecError::InvalidRow)
    ));
}
