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
        variable_value::{zoned_datetime::ZonedDateTime, VariableValue},
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{AutoFutureQueueConsumer, ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use super::{contains_data, IGNORED_ROW_SIGNATURE};
use crate::QueryTestConfig;

use self::data::get_bootstrap_data;

mod data;
mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

async fn bootstrap_query(query: &ContinuousQuery, effective_from: u64) {
    let data = get_bootstrap_data(effective_from);

    for change in data {
        let _ = query.process_source_change(change).await;
    }
}

pub async fn not_reported(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::not_reported_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let init_time =
        NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let time0 = init_time.and_utc().timestamp_millis() as u64;
    let time1 = time0 + Duration::minutes(30).num_milliseconds() as u64;
    let time2 = time1 + Duration::minutes(30).num_milliseconds() as u64;
    let time3 = time2 + Duration::minutes(30).num_milliseconds() as u64;
    let time4 = time3 + Duration::minutes(30).num_milliseconds() as u64;

    now_override.store(time0, Ordering::Relaxed);

    bootstrap_query(&cq, time0).await;

    //jump forward 30 minutes, add sensor value for Turbine 1 / RPM
    {
        now_override.store(time1, Ordering::Relaxed);

        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv1"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv1"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e1-s1"),
                out_node: ElementReference::new("test", "sv1"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        assert_eq!(result, vec![]);
    }

    //jump forward another 30 minutes
    {
        now_override.store(time2, Ordering::Relaxed);

        let mut results = Vec::new();
        for _ in 0..3 {
            let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
            results.extend(result);
        }
        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None); // no more results
        assert_eq!(results.len(), 3);

        assert!(contains_data(
            &results,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 1".into()),
                    "sensor" => VariableValue::String("Temp".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time0 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));

        assert!(contains_data(
            &results,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 2".into()),
                    "sensor" => VariableValue::String("Temp".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time0 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));

        assert!(contains_data(
            &results,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 2".into()),
                    "sensor" => VariableValue::String("RPM".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time0 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //add sensor value for Turbine 2 / RPM
    {
        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv2"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time2,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv2"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time2,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e2-s1"),
                out_node: ElementReference::new("test", "sv2"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Removing {
                before: variablemap!(
                    "equipment" => VariableValue::String("Turbine 2".into()),
                    "sensor" => VariableValue::String("RPM".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(init_time.and_utc().timestamp_millis()))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //jump forward another 30 minutes
    {
        now_override.store(time3, Ordering::Relaxed);
        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None); // no additional results
        assert_eq!(result.len(), 1);

        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 1".into()),
                    "sensor" => VariableValue::String("RPM".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time1 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //add sensor value for for each sensor
    {
        let mut changes = Vec::new();

        changes.push(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv11"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        });
        changes.push(SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv11"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e1-s1"),
                out_node: ElementReference::new("test", "sv11"),
            },
        });
        changes.push(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv12"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 90
                })),
            },
        });
        changes.push(SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv12"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e1-s2"),
                out_node: ElementReference::new("test", "sv12"),
            },
        });

        changes.push(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv13"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        });
        changes.push(SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv13"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e2-s1"),
                out_node: ElementReference::new("test", "sv13"),
            },
        });
        changes.push(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv14"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 90
                })),
            },
        });
        changes.push(SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv14"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time3,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e2-s2"),
                out_node: ElementReference::new("test", "sv14"),
            },
        });

        for change in changes {
            _ = cq.process_source_change(change).await.unwrap();
        }
    }

    //jump forward another 30 minutes
    {
        now_override.store(time4, Ordering::Relaxed);
        assert_eq!(fqc.recv(std::time::Duration::from_millis(1000)).await, None);
    }
}

pub async fn not_reported_with_true_now_or_later(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::not_reported_query_v2(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let init_time =
        NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let time0 = init_time.and_utc().timestamp_millis() as u64;
    let time1 = time0 + Duration::minutes(30).num_milliseconds() as u64;
    let time2 = time1 + Duration::minutes(30).num_milliseconds() as u64;

    now_override.store(time0, Ordering::Relaxed);

    bootstrap_query(&cq, time0).await;

    //jump forward 30 minutes, add sensor value for Turbine 1 / RPM
    {
        now_override.store(time1, Ordering::Relaxed);

        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv1"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv1"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e1-s1"),
                out_node: ElementReference::new("test", "sv1"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        assert_eq!(result, vec![]);
    }

    //jump forward another 30 minutes
    {
        now_override.store(time2, Ordering::Relaxed);

        let mut results = Vec::new();
        for _ in 0..3 {
            let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
            results.extend(result);
        }
        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None); // no more results
        assert_eq!(results.len(), 3);

        assert!(contains_data(
            &results,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 1".into()),
                    "sensor" => VariableValue::String("Temp".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time0 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));

        assert!(contains_data(
            &results,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 2".into()),
                    "sensor" => VariableValue::String("Temp".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time0 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));

        assert!(contains_data(
            &results,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "equipment" => VariableValue::String("Turbine 2".into()),
                    "sensor" => VariableValue::String("RPM".into()),
                    "last_ts" => VariableValue::ZonedDateTime(ZonedDateTime::from_epoch_millis(time0 as i64))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn percent_not_reported(config: &(impl QueryTestConfig + Send)) {
    let cq = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::percent_not_reported_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        Arc::new(builder.build().await)
    };

    let now_override = Arc::new(AtomicU64::new(0));
    let fqc =
        Arc::new(AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now_override.clone()));
    cq.set_future_consumer(fqc.clone()).await;

    let init_time =
        NaiveDateTime::new(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), NaiveTime::MIN);
    let time0 = init_time.and_utc().timestamp_millis() as u64;
    let time1 = time0 + Duration::minutes(60).num_milliseconds() as u64;
    let time2 = time1 + Duration::minutes(30).num_milliseconds() as u64;
    let time3 = time2 + Duration::minutes(30).num_milliseconds() as u64;
    let time4 = time3 + Duration::minutes(30).num_milliseconds() as u64;

    now_override.store(time0, Ordering::Relaxed);

    bootstrap_query(&cq, time0).await;

    //jump forward 60 minutes
    {
        now_override.store(time1, Ordering::Relaxed);

        let mut results = Vec::new();
        for _ in 0..2 {
            let result = fqc
                .recv(std::time::Duration::from_secs(5))
                .await
                .unwrap_or_default();
            results.extend(result);
        }

        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None); // no more results
        assert_eq!(results.len(), 2);
        assert!(results[0].data_eq(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(0.0))
            ),
            after: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(50.0))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }));
        assert!(results[1].data_eq(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(50.0))
            ),
            after: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(100.0))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }));
    }

    //add sensor value for Turbine 1 - RPM
    {
        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv1"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv1"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e1-s1"),
                out_node: ElementReference::new("test", "sv1"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "percent_not_reporting" => VariableValue::from(json!(100.0))
                ),
                after: variablemap!(
                    "percent_not_reporting" => VariableValue::from(json!(50.0))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    //add sensor value for Turbine 1 - Temp
    {
        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv2"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 60
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv2"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time1,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e1-s2"),
                out_node: ElementReference::new("test", "sv2"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        //println!("result: {:?}", result);
        assert_eq!(result, vec![]);
    }

    //jump forward another 30 minutes
    {
        now_override.store(time2, Ordering::Relaxed);
        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None);
        // no more results
    }

    //add sensor value for Turbine 2 / RPM
    {
        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv3"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time2,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv3"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time2,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e2-s1"),
                out_node: ElementReference::new("test", "sv3"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(result[0].data_eq(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(50.0))
            ),
            after: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(0.0))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }));
    }

    //jump forward another 30 minutes
    {
        now_override.store(time3, Ordering::Relaxed);
        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None); // no additional results

        assert_eq!(result.len(), 1);
        assert!(result[0].data_eq(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(0.0))
            ),
            after: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(50.0))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }));
    }

    //jump forward another 30 minutes
    {
        now_override.store(time4, Ordering::Relaxed);
        let result = fqc.recv(std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(fqc.recv(std::time::Duration::from_millis(100)).await, None); // no additional results

        assert_eq!(result.len(), 1);
        assert!(result[0].data_eq(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(50.0))
            ),
            after: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(100.0))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }));
    }

    println!("-----------------------------------");
    //delete all equipment 2 values, add 1 back
    {
        _ = cq
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv3"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time4,
                },
            })
            .await
            .unwrap();

        _ = cq
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "e2-s1-v1"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time4,
                },
            })
            .await
            .unwrap();

        _ = cq
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "e2-s2-v1"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time4,
                },
            })
            .await
            .unwrap();

        let value_node = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "sv4"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: time2,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 3000
                })),
            },
        };

        let value_rel = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r-sv4"),
                    labels: Arc::new([Arc::from("HAS_VALUE")]),
                    effective_from: time2,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "e2-s2"),
                out_node: ElementReference::new("test", "sv4"),
            },
        };

        _ = cq.process_source_change(value_node.clone()).await.unwrap();

        let result = cq.process_source_change(value_rel.clone()).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(result[0].data_eq(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(100.0))
            ),
            after: variablemap!(
                "percent_not_reporting" => VariableValue::from(json!(50.0))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }));
    }
}
