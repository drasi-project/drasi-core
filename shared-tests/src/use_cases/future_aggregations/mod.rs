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

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::FunctionRegistry,
        variable_value::{float::Float, VariableValue},
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{AutoFutureQueueConsumer, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn truefor_sum(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::truefor_sum_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let inv_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = inv_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //create order 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "ORD1"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderNumber": "ORD1",
                    "status": "ready",
                    "value": 2
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 0);
    }

    //jump to 5 seconds later
    {
        println!("-----------------later---------------------");
        now += Duration::seconds(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();

        assert_eq!(result.len(), 1);
        //println!("Result: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            after: variablemap!("value"=>VariableValue::Float(Float::from(2.0))),
            before: None,
            grouping_keys: vec![],
            default_before: false,
            default_after: false
        }));
    }

    //create order 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "ORD2"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderNumber": "ORD2",
                    "status": "ready",
                    "value": 1
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("Result: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            after: variablemap!("value"=>VariableValue::Float(Float::from(3.0))),
            before: Some(variablemap!("value"=>VariableValue::Float(Float::from(2.0))),),
            grouping_keys: vec![],
            default_before: true,
            default_after: false
        }));
    }
}


#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn truefor_grouped_sum(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::truefor_sum_grouping_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let inv_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = inv_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //create order 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "ORD1"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderNumber": "ORD1",
                    "status": "ready",
                    "category": "A",
                    "value": 2
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 0);
    }

    //jump to 5 seconds later
    {
        println!("-----------------later---------------------");
        now += Duration::seconds(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();

        assert_eq!(result.len(), 1);
        //println!("Result: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            after: variablemap!(
                "value"=>VariableValue::Float(Float::from(2.0)),
                "category"=>VariableValue::from("A")
            ),
            before: None,
            grouping_keys: vec!["category".into()],
            default_before: false,
            default_after: false
        }));
    }

    //create order 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "ORD2"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderNumber": "ORD2",
                    "status": "ready",
                    "category": "A",
                    "value": 1
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("Result: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            after: variablemap!(
                "value"=>VariableValue::Float(Float::from(3.0)),
                "category"=>VariableValue::from("A")
            ),
            before: Some(variablemap!(
                "value"=>VariableValue::Float(Float::from(2.0)),
                "category"=>VariableValue::from("A")
            )),
            grouping_keys: vec!["category".into()],
            default_before: true,
            default_after: false
        }));
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn truelater_max(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::truelater_max_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let inv_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = inv_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //create Issue 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Issue")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "state": "open",
                    "created_at": "2020-01-01T00:00:00Z"
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 0);
    }

    //jump to 10 seconds later
    {
        println!("-----------------later---------------------");
        now += Duration::seconds(10).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();

        assert_eq!(result.len(), 1);
        println!("Result: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!("id"=>VariableValue::from("i1"))
        }));
    }

    //create Comment for Issue 1
    {
        cq.process_source_change(SourceChange::Insert { 
            element: Element::Relation { 
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: now,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "c1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r1"
                }))
            }
        }).await.unwrap();

        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c1"),
                    labels: Arc::new([Arc::from("Comment")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c1",
                    "created_at": "2020-01-01T00:00:10Z"
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("Result: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!("id"=>VariableValue::from("i1"))
        }));
    }


}