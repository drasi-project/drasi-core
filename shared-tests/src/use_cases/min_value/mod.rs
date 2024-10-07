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
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

pub async fn min_value(config: &(impl QueryTestConfig + Send)) {
    let min_query = {
        let mut builder = QueryBuilder::new(queries::min_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add initial value
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t1"),
                    labels: Arc::new([Arc::from("Thing")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({ "Value": 5 })),
            },
        };

        let result = min_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        //println!("Node Result - Add t1: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
                "min_value" => VariableValue::Null
            )),
            after: variablemap!(
              "min_value" => VariableValue::from(json!(5.0))
            ),
        }));
    }

    //Add lower value
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t3"),
                    labels: Arc::new([Arc::from("Thing")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(json!({ "Value": 3 })),
            },
        };

        let result = min_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        //println!("Node Result - Add t3: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
              "min_value" => VariableValue::from(json!(5.0))
            )),
            after: variablemap!(
              "min_value" => VariableValue::from(json!(3.0))
            ),
        }));
    }

    //Increment lower value
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t3"),
                    labels: Arc::new([Arc::from("Thing")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(json!({ "Value": 4 })),
            },
        };

        let result = min_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            before: Some(variablemap!(
              "min_value" => VariableValue::from(json!(3.0))
            )),
            after: variablemap!(
              "min_value" => VariableValue::from(json!(4.0))
            ),
        }));
    }

    //Increment higher value
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t1"),
                    labels: Arc::new([Arc::from("Thing")]),
                    effective_from: 4000,
                },
                properties: ElementPropertyMap::from(json!({ "Value": 6 })),
            },
        };

        let result = min_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result, vec![]);
    }
}
