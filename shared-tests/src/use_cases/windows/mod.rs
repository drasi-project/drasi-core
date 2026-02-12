#![allow(clippy::unwrap_used)]
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

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn sliding_window_max(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::sliding_window_max_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let eff_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = eff_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //Add sensor 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s1"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s1",
                    "value": 50
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!("slidingMax"=>VariableValue::Null)),
                after: variablemap!("slidingMax"=>VariableValue::Float(Float::from(50.0))),
                grouping_keys: vec![],
                default_before: true,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 5 minutes later, add sensor 2
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s2"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s2",
                    "value": 30
                })),
            },
        };

        _ = cq.process_source_change(change.clone()).await.unwrap();
    }

    //jump to 5 minutes later
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!("slidingMax"=>VariableValue::Float(Float::from(50.0)))),
                after: variablemap!("slidingMax"=>VariableValue::Float(Float::from(30.0))),
                grouping_keys: vec![],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 2 minutes later, add sensor 3
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(2).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s3"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s3",
                    "value": 45
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!("slidingMax"=>VariableValue::Float(Float::from(30.0)))),
                after: variablemap!("slidingMax"=>VariableValue::Float(Float::from(45.0))),
                grouping_keys: vec![],
                default_before: true,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 2 minutes later, update sensor 2
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(2).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s2"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s2",
                    "value": 65
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!("slidingMax"=>VariableValue::Float(Float::from(45.0)))),
                after: variablemap!("slidingMax"=>VariableValue::Float(Float::from(65.0))),
                grouping_keys: vec![],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 8 minutes later
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(8).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(2)).await;
        assert_eq!(result, None);
    }

    //jump to 2 minutes later
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(2).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!("slidingMax"=>VariableValue::Float(Float::from(65.0)))),
                after: variablemap!("slidingMax"=>VariableValue::Null),
                grouping_keys: vec![],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 2 minutes later, update sensor 2
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(2).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s2"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s2",
                    "value": 15
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!("slidingMax"=>VariableValue::Null)),
                after: variablemap!("slidingMax"=>VariableValue::Float(Float::from(15.0))),
                grouping_keys: vec![],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn sliding_window_avg_grouped(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::sliding_window_avg_grouped_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let eff_date = NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let mut now = eff_date.and_utc().timestamp_millis() as u64;
    now_override.store(now, Ordering::Relaxed);

    //Add sensor 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s1"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s1",
                    "group": "A",
                    "value": 50
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(0.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(50.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: true,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 5 minutes later, add sensor 2
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s2"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s2",
                    "group": "A",
                    "value": 30
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(50.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(40.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: true,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //Add sensor 3
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s3"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s3",
                    "group": "B",
                    "value": 10
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(0.0)),
                    "sensorGroup"=>VariableValue::String("B".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(10.0)),
                    "sensorGroup"=>VariableValue::String("B".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: true,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 5 minutes later
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(40.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(30.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump to 5 minutes later
    {
        println!("-----------------later---------------------");
        now += Duration::minutes(5).num_milliseconds() as u64;
        now_override.store(now, Ordering::Relaxed);

        let result = {
            let r1 = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
            let r2 = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
            let mut combined = r1;
            combined.extend(r2);
            combined
        };
        assert_eq!(result.len(), 2);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(10.0)),
                    "sensorGroup"=>VariableValue::String("B".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(0.0)),
                    "sensorGroup"=>VariableValue::String("B".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(30.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(0.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //update sensor 2
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "s2"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: now,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "s2",
                    "group": "A",
                    "value": 5
                })),
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(0.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(5.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //delete sensor 2
    {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "s2"),
                labels: Arc::new([Arc::from("Sensor")]),
                effective_from: now,
            },
        };

        let result = cq.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        println!("result: {result:#?}");

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(5.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                )),
                after: variablemap!(
                    "slidingAvg"=>VariableValue::Float(Float::from(0.0)),
                    "sensorGroup"=>VariableValue::String("A".to_string())
                ),
                grouping_keys: vec!["sensorGroup".to_string()],
                default_before: false,
                default_after: true,
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}
