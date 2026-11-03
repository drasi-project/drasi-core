// Copyright 2025 The Drasi Authors.
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

#![allow(clippy::unwrap_used)]
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeDelta};
use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::FunctionRegistry,
        variable_value::{
            duration::Duration, zoned_time::ZonedTime, ListRange, RangeBound, VariableValue,
        },
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use super::{contains_data, IGNORED_ROW_SIGNATURE};
use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

pub async fn before_value(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::increasing_value_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add invoice 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i2"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i2",
                    "amount": 200,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //update invoice 1 with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 105,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "amount" => VariableValue::from(json!(105.0)),
                  "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //update invoice 1 with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 110,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "amount" => VariableValue::from(json!(105.0)),
                  "id" => VariableValue::from(json!("i1"))
                ),
                after: variablemap!(
                  "amount" => VariableValue::from(json!(110.0)),
                  "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //update invoice 1 with decreasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 90,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Removing {
                before: variablemap!(
                  "amount" => VariableValue::from(json!(110.0)),
                  "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //update invoice 1 with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 95,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "amount" => VariableValue::from(json!(95.0)),
                  "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}

pub async fn before_sum(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::increasing_sum_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "l1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //insert invoice line with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "l1"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "l1",
                    "amount": 10,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //Add relation 2
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r2"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "l2"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r2",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //insert invoice line with decreasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "l2"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "l2",
                    "amount": -5,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Removing {
                before: variablemap!(
                  "id" => VariableValue::from(json!("i1"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}

async fn build_query(config: &(impl QueryTestConfig + Send), query: &str) -> ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let mut builder = QueryBuilder::new(query, parser).with_function_registry(function_registry);
    builder = config.config_query(builder).await;
    builder.build().await
}

fn thing(effective_from: u64, properties: serde_json::Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn date(year: i32, month: u32, day: u32) -> VariableValue {
    VariableValue::Date(NaiveDate::from_ymd_opt(year, month, day).unwrap())
}

fn noon() -> NaiveTime {
    NaiveTime::from_hms_opt(12, 0, 0).unwrap()
}

fn list_range(start: RangeBound, end: RangeBound) -> VariableValue {
    VariableValue::ListRange(ListRange { start, end })
}

/// Values other than JSON types and datetimes survive in the state of the
/// before functions, also inside lists and maps and as the default argument.
pub async fn value_types(config: &(impl QueryTestConfig + Send)) {
    for query in [
        queries::previous_value_types_query(),
        queries::previous_distinct_value_types_query(),
    ] {
        value_types_for(config, query).await;
    }
    distinct_date_unchanged(config).await;
}

async fn value_types_for(config: &(impl QueryTestConfig + Send), query: &str) {
    let cq = build_query(config, query).await;

    let inserted = thing(
        1000,
        json!({
            "id": "t1",
            "d": "2024-01-01",
            "t": "12:00:00",
            "zt": "12:00:00+01:00",
            "dur": "P1Y2M3DT4H",
            "tag": "Date",
            "lo": 1,
            "hi": 3,
        }),
    );
    let defaults = variablemap!(
      "id" => VariableValue::from(json!("t1")),
      "date" => VariableValue::Null,
      "local_time" => VariableValue::Null,
      "zoned_time" => VariableValue::Null,
      "duration" => VariableValue::Null,
      "date_list" => VariableValue::Null,
      "date_map" => VariableValue::Null,
      "node" => VariableValue::Null,
      "date_with_default" => date(2000, 1, 1),
      "tagged_map" => VariableValue::Null,
      "range" => VariableValue::Null,
      "range_list" => VariableValue::Null
    );

    {
        let change = SourceChange::Insert {
            element: inserted.clone(),
        };
        let result = cq.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1, "{query}");
        assert!(
            contains_data(
                &result,
                &QueryPartEvaluationContext::Adding {
                    after: defaults.clone(),
                    row_signature: IGNORED_ROW_SIGNATURE,
                }
            ),
            "{query}\n{result:?}"
        );
    }

    {
        let change = SourceChange::Update {
            element: thing(
                2000,
                json!({
                    "id": "t1",
                    "d": "2024-01-02",
                    "t": "13:00:00",
                    "zt": "13:00:00+02:00",
                    "dur": "P2Y",
                    "tag": "Node",
                    "lo": 2,
                    "hi": 4,
                }),
            ),
        };
        let result = cq.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1, "{query}");
        assert!(
            contains_data(
                &result,
                &QueryPartEvaluationContext::Updating {
                    before: defaults,
                    after: variablemap!(
                      "id" => VariableValue::from(json!("t1")),
                      "date" => date(2024, 1, 1),
                      "local_time" => VariableValue::LocalTime(noon()),
                      "zoned_time" => VariableValue::ZonedTime(ZonedTime::new(
                          noon(),
                          FixedOffset::east_opt(3600).unwrap()
                      )),
                      "duration" => VariableValue::Duration(Duration::new(
                          TimeDelta::days(3) + TimeDelta::hours(4),
                          1,
                          2
                      )),
                      "date_list" => VariableValue::List(vec![date(2024, 1, 1)]),
                      "date_map" => VariableValue::Object(BTreeMap::from([(
                          "d".to_string(),
                          date(2024, 1, 1)
                      )])),
                      "node" => VariableValue::Element(Arc::new(inserted)),
                      "date_with_default" => date(2024, 1, 1),
                      "tagged_map" => VariableValue::Object(BTreeMap::from([(
                          "__drasi_state_type__".to_string(),
                          VariableValue::from(json!("Date"))
                      )])),
                      "range" => list_range(RangeBound::Index(1), RangeBound::Index(3)),
                      "range_list" => VariableValue::List(vec![
                          list_range(RangeBound::Index(1), RangeBound::Unbounded),
                          list_range(RangeBound::Unbounded, RangeBound::Index(3)),
                      ])
                    ),
                    row_signature: IGNORED_ROW_SIGNATURE,
                }
            ),
            "{query}\n{result:?}"
        );
    }
}

async fn distinct_date_unchanged(config: &(impl QueryTestConfig + Send)) {
    let cq = build_query(config, queries::previous_distinct_date_query()).await;

    {
        let change = SourceChange::Insert {
            element: thing(1000, json!({ "id": "t1", "d": "2024-01-01", "other": "a" })),
        };
        let result = cq.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "id" => VariableValue::from(json!("t1")),
                  "date" => VariableValue::Null
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    {
        let change = SourceChange::Update {
            element: thing(2000, json!({ "id": "t1", "d": "2024-01-02", "other": "a" })),
        };
        let result = cq.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "id" => VariableValue::from(json!("t1")),
                  "date" => VariableValue::Null
                ),
                after: variablemap!(
                  "id" => VariableValue::from(json!("t1")),
                  "date" => date(2024, 1, 1)
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    // Another property changes while the date stays the same.
    {
        let change = SourceChange::Update {
            element: thing(3000, json!({ "id": "t1", "d": "2024-01-02", "other": "b" })),
        };
        let result = cq.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 0, "{result:?}");
    }
}
