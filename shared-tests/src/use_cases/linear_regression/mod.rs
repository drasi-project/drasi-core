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
        context::QueryPartEvaluationContext,
        functions::FunctionRegistry,
        variable_value::{float::Float, VariableValue},
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
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
pub async fn linear_gradient(config: &(impl QueryTestConfig + Send)) {
    let lg_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::gradient_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add initial values
    {
        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p1"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 1, "y": 6 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p2"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 2, "y": 7 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p3"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 3, "y": 8 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p4"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 4, "y": 10 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p5"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 5, "y": 12 })),
                },
            })
            .await
            .unwrap();

        let result = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p6"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 6, "y": 50 })),
                },
            })
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        println!("Node Result - Add p6: {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
                "Gradient" => VariableValue::Float(Float::from_f64(1.5).unwrap())
            )),
            after: variablemap!(
              "Gradient" => VariableValue::Float(Float::from_f64(6.771428571428571).unwrap())
            ),
        }));
    }

    // remove p2
    {
        let result = lg_query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    effective_from: 0,
                    labels: Arc::new([Arc::from("Point")]),
                },
            })
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        println!("Node Result - Remove p2: {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: false,
            default_after: true,
            before: Some(variablemap!(
                "Gradient" => VariableValue::Float(Float::from_f64(6.771428571428571).unwrap())
            )),
            after: variablemap!(
              "Gradient" => VariableValue::Float(Float::from_f64(6.972972972972973).unwrap())
            ),
        }));
    }

    // remove p6
    {
        let result = lg_query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p6"),
                    effective_from: 0,
                    labels: Arc::new([Arc::from("Point")]),
                },
            })
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        println!("Node Result - Remove p6: {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: false,
            default_after: true,
            before: Some(variablemap!(
                "Gradient" => VariableValue::Float(Float::from_f64(6.972972972972973).unwrap())
            )),
            after: variablemap!(
              "Gradient" => VariableValue::Float(Float::from_f64(1.4857142857142858).unwrap())
            ),
        }));
    }

    //remove p1, p3, p4, p5
    {
        let result = lg_query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    effective_from: 0,
                    labels: Arc::new([Arc::from("Point")]),
                },
            })
            .await
            .unwrap();
        println!("Node Result - Remove p1: {result:?}");

        let result = lg_query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p3"),
                    effective_from: 0,
                    labels: Arc::new([Arc::from("Point")]),
                },
            })
            .await
            .unwrap();
        println!("Node Result - Remove p3: {result:?}");

        let result = lg_query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p4"),
                    effective_from: 0,
                    labels: Arc::new([Arc::from("Point")]),
                },
            })
            .await
            .unwrap();
        println!("Node Result - Remove p4: {result:?}");

        let result = lg_query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p5"),
                    effective_from: 0,
                    labels: Arc::new([Arc::from("Point")]),
                },
            })
            .await
            .unwrap();
        println!("Node Result - Remove p5: {result:?}");
    }

    //re-add initial values
    {
        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p1"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 1, "y": 6 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p2"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 2, "y": 7 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p3"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 3, "y": 8 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p4"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 4, "y": 10 })),
                },
            })
            .await
            .unwrap();

        _ = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p5"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 5, "y": 12 })),
                },
            })
            .await
            .unwrap();

        let result = lg_query
            .process_source_change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p6"),
                        labels: Arc::new([Arc::from("Point")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({ "x": 6, "y": 50 })),
                },
            })
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        println!("Node Result - Add p6: {result:?}");
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
                "Gradient" => VariableValue::Float(Float::from_f64(1.5).unwrap())
            )),
            after: variablemap!(
              "Gradient" => VariableValue::Float(Float::from_f64(6.771428571428571).unwrap())
            ),
        }));
    }
}
