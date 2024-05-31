use std::sync::Arc;

use serde_json::json;

use drasi_core::{
    evaluation::{context::PhaseEvaluationContext, variable_value::VariableValue},
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
    let mq = Arc::new(queries::min_query());
    let min_query = {
        let mut builder = QueryBuilder::new(mq.clone());
        builder = config.config_query(builder, mq.clone()).await;
        builder.build()
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
        assert!(result.contains(&PhaseEvaluationContext::Aggregation {
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
        assert!(result.contains(&PhaseEvaluationContext::Aggregation {
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
        assert!(result.contains(&PhaseEvaluationContext::Aggregation {
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
