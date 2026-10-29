// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{FixedOffset, NaiveDate, TimeZone};
use drasi_core_types::data_value::DataValue;
use drasi_query_cypher::CypherParser;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::FunctionRegistry,
        variable_value::{integer::Integer, zoned_datetime::ZonedDateTime, VariableValue},
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

fn element(id: &str, values: &QueryVariables, effective_from: u64) -> Element {
    let mut properties = ElementPropertyMap::new();
    for (key, value) in values {
        properties.insert(key, value.try_into().unwrap());
    }
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("derived", id),
            labels: Arc::from([Arc::from("Result")]),
            effective_from,
        },
        properties,
    }
}

fn encoded(values: &QueryVariables) -> BTreeMap<Box<str>, DataValue> {
    values
        .iter()
        .map(|(name, value)| (name.clone(), DataValue::encode(value).unwrap()))
        .collect()
}

#[tokio::test]
async fn query_preserves_rich_properties_through_add_update_and_remove() {
    let functions = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(functions));
    let query = QueryBuilder::new(
        "MATCH (r:Result) RETURN r.huge AS huge, r.day AS day, r.zone AS zone",
        parser,
    )
    .build()
    .await;
    let zone = FixedOffset::east_opt(3600)
        .unwrap()
        .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
        .unwrap();
    let mut values = BTreeMap::from([
        (
            "huge".into(),
            VariableValue::Integer(Integer::from(u64::MAX)),
        ),
        (
            "day".into(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()),
        ),
        (
            "zone".into(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(zone, Some("Etc/GMT-1".to_string()))),
        ),
    ]);
    let changes = query
        .process_source_change(SourceChange::Insert {
            element: element("row-1", &values, 10),
        })
        .await
        .unwrap();
    let [QueryPartEvaluationContext::Adding {
        after,
        row_signature,
    }] = changes.as_slice()
    else {
        panic!("expected one added row, got {changes:?}");
    };
    let identity = *row_signature;
    assert_eq!(encoded(after), encoded(&values));
    let before = values.clone();
    values.insert(
        "huge".into(),
        VariableValue::Integer(Integer::from(u64::MAX - 1)),
    );
    let changes = query
        .process_source_change(SourceChange::Update {
            removed_properties: Default::default(),
            element: element("row-1", &values, 20),
        })
        .await
        .unwrap();
    let [QueryPartEvaluationContext::Updating {
        before: actual_before,
        after,
        row_signature,
    }] = changes.as_slice()
    else {
        panic!("expected one updated row, got {changes:?}");
    };
    assert_eq!(*row_signature, identity);
    assert_eq!(encoded(actual_before), encoded(&before));
    assert_eq!(encoded(after), encoded(&values));
    let changes = query
        .process_source_change(SourceChange::Delete {
            metadata: element("row-1", &values, 30).get_metadata().clone(),
        })
        .await
        .unwrap();
    let [QueryPartEvaluationContext::Removing {
        before,
        row_signature,
    }] = changes.as_slice()
    else {
        panic!("expected one removed row, got {changes:?}");
    };
    assert_eq!(*row_signature, identity);
    assert_eq!(encoded(before), encoded(&values));
}

#[tokio::test]
async fn equal_rich_values_keep_distinct_result_identities() {
    let functions = Arc::new(FunctionRegistry::new());
    let query = QueryBuilder::new(
        "MATCH (r:Result) RETURN r.huge AS huge",
        Arc::new(CypherParser::new(functions)),
    )
    .build()
    .await;
    let values = BTreeMap::from([(
        "huge".into(),
        VariableValue::Integer(Integer::from(u64::MAX)),
    )]);
    let mut identities = Vec::new();
    for id in ["row-1", "row-2"] {
        let changes = query
            .process_source_change(SourceChange::Insert {
                element: element(id, &values, 10),
            })
            .await
            .unwrap();
        let [QueryPartEvaluationContext::Adding {
            after,
            row_signature,
        }] = changes.as_slice()
        else {
            panic!("expected one added row, got {changes:?}");
        };
        assert_eq!(encoded(after), encoded(&values));
        identities.push(*row_signature);
    }
    assert_ne!(identities[0], identities[1]);
    let changes = query
        .process_source_change(SourceChange::Delete {
            metadata: element("row-1", &values, 20).get_metadata().clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Removing { row_signature, .. }]
            if *row_signature == identities[0]
    ));
    let changes = query
        .process_source_change(SourceChange::Delete {
            metadata: element("row-2", &values, 20).get_metadata().clone(),
        })
        .await
        .unwrap();
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Removing { row_signature, .. }]
            if *row_signature == identities[1]
    ));
}
