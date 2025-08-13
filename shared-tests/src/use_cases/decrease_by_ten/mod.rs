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

use std::sync::Arc;

use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use self::data::get_bootstrap_data;
use crate::QueryTestConfig;

mod data;
mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

async fn bootstrap_query(query: &ContinuousQuery) {
    let data = get_bootstrap_data();

    for change in data {
        let _ = query.process_source_change(change).await;
    }
}

// Query identifies a products daily revenue has decreased by $10,000.
pub async fn decrease_by_ten(config: &(impl QueryTestConfig + Send)) {
    let decrease_by_ten_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::decrease_by_ten_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::decrease_by_ten_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&decrease_by_ten_query).await;

    let mut timestamp = 1696194000; // 2023-10-01 21:00:00

    // Add an initial daily revenue of $100K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 100000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Add initial revenue amount of $100k ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    // Add a greater daily revenue of $105K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 105000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Increase daily revenue amount to $105K ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    // Add a lesser revenue of $101K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 101000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Decrease daily revenue amount to $101k({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    // Add a lesser revenue of $90K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 90000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Decrease daily revenue amount to $90K ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(101000.0)),
              "dailyRevenue" => VariableValue::from(json!(90000.0))
            )
        }));

        timestamp += 24 * 60 * 60;
    }

    // Add a lower revenue of $75K. Result will be updated in the query result.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 75000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Decrease daily revenue amount to $80k ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(101000.0)),
              "dailyRevenue" => VariableValue::from(json!(90000.0))
            ),
            after: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(90000.0)),
              "dailyRevenue" => VariableValue::from(json!(75000.0))
            )
        }));

        timestamp += 24 * 60 * 60;
    }

    // Add a same revenue of $75K. Result will be removed from the query result.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 90000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update with same daily revenue amount of $80k ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(90000.0)),
              "dailyRevenue" => VariableValue::from(json!(75000.0))
            )
        }));
    }
}

pub async fn decrease_by_ten_percent(config: &(impl QueryTestConfig + Send)) {
    let decrease_by_ten_query = {
        let parser = Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new())));
        let mut builder = QueryBuilder::new(queries::decrease_by_ten_percent_query(), parser)
            .with_joins(queries::decrease_by_ten_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&decrease_by_ten_query).await;

    let mut timestamp = 1696194000; // 2023-10-01 21:00:00

    // Add an initial daily revenue of $100K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 100000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Add initial revenue amount of $100k ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    // Add a greater daily revenue of $105K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 105000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Increase daily revenue amount to $105K ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    // Add a lesser revenue of $101K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 101000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Decrease daily revenue amount to $101k({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 24 * 60 * 60;
    }

    // Add a lesser revenue of $90K.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 90000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Decrease daily revenue amount to $90K ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(101000.0)),
              "dailyRevenue" => VariableValue::from(json!(90000.0))
            )
        }));

        timestamp += 24 * 60 * 60;
    }

    // Add a lower revenue of $75K. Result will be updated in the query result.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 75000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Decrease daily revenue amount to $80k ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(101000.0)),
              "dailyRevenue" => VariableValue::from(json!(90000.0))
            ),
            after: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(90000.0)),
              "dailyRevenue" => VariableValue::from(json!(75000.0))
            )
        }));

        timestamp += 24 * 60 * 60;
    }

    // Add a same revenue of $75K. Result will be removed from the query result.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.Dales", "prod_01_daily_revenue"),
                    labels: Arc::new([Arc::from("DailyRevenue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "prod_01_daily_revenue", "prod_id": "prod_01", "timestamp": timestamp, "amount": 90000.0  }),
                ),
            },
        };

        let result = decrease_by_ten_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update with same daily revenue amount of $80k ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "productId" => VariableValue::from(json!("prod_01")),
              "productManagerId" => VariableValue::from(json!("emp_01")),
              "previousDailyRevenue" => VariableValue::from(json!(90000.0)),
              "dailyRevenue" => VariableValue::from(json!(75000.0))
            )
        }));
    }
}
