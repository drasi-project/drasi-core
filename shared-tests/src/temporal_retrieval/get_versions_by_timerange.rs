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

use crate::QueryTestConfig;
use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;
use serde_json::json;

macro_rules! variablemap {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::BTreeMap::new();
         $( map.insert($key.to_string().into(), $val); )*
         map
    }}
  }

pub fn test_query() -> &'static str {
    "MATCH 
        (t:Thing)
    RETURN
        t,
        drasi.getVersionsByTimeRange(t, 1000, 2001) as range
    "
}

pub async fn get_versions_by_timerange(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder =
            QueryBuilder::new(test_query(), parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let v0 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 0 })),
    };

    let v1 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 1000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 1 })),
    };

    let v2 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 2000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 2 })),
    };

    let v3 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 3000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 3 })),
    };

    // bootstrap
    {
        _ = query
            .process_source_change(SourceChange::Insert {
                element: v0.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v1.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v2.clone(),
            })
            .await
            .unwrap();
    }

    let change = SourceChange::Update {
        element: v3.clone(),
    };

    let result = query.process_source_change(change.clone()).await.unwrap();
    assert_eq!(result.len(), 1);

    let after = match result[0] {
        QueryPartEvaluationContext::Updating { ref after, .. } => after,
        _ => panic!("Expected Updating"),
    };

    assert_eq!(
        after,
        &variablemap!(
            "t" => v3.to_expression_variable(),
            "range" => VariableValue::List(vec![
                v1.to_expression_variable(),
                v2.to_expression_variable()
            ])
        )
    );
}

pub fn test_query_with_initial_value_flag() -> &'static str {
    "MATCH 
        (t:Thing)
    RETURN
        t,
        drasi.getVersionsByTimeRange(t,1111, 2001, true) as range
    "
}

pub fn test_query_with_initial_value_flag_test_2() -> &'static str {
    "MATCH 
        (t:Thing)
    RETURN
        t,
        drasi.getVersionsByTimeRange(t,1750,2000, true) as range
    "
}

pub async fn get_versions_by_timerange_with_initial_value_flag(
    config: &(impl QueryTestConfig + Send),
) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(test_query_with_initial_value_flag(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let v0 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 0 })),
    };

    let v1 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 1111,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 1 })),
    };

    let v2 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 2000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 2 })),
    };

    let v3 = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "t1"),
            labels: Arc::new([Arc::from("Thing")]),
            effective_from: 3000,
        },
        properties: ElementPropertyMap::from(json!({ "Value": 3 })),
    };

    // bootstrap
    {
        _ = query
            .process_source_change(SourceChange::Insert {
                element: v0.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v1.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v2.clone(),
            })
            .await
            .unwrap();
    }

    let change = SourceChange::Update {
        element: v3.clone(),
    };

    let result = query.process_source_change(change.clone()).await.unwrap();
    assert_eq!(result.len(), 1);

    let after = match result[0] {
        QueryPartEvaluationContext::Updating { ref after, .. } => after,
        _ => panic!("Expected Updating"),
    };

    assert_eq!(
        after,
        &variablemap!(
            "t" => v3.to_expression_variable(),
            "range" => VariableValue::List(vec![
                v1.to_expression_variable(),
                v2.to_expression_variable(),
            ])
        )
    );

    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(test_query_with_initial_value_flag_test_2(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // bootstrap
    {
        _ = query
            .process_source_change(SourceChange::Insert {
                element: v0.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v1.clone(),
            })
            .await
            .unwrap();

        _ = query
            .process_source_change(SourceChange::Update {
                element: v2.clone(),
            })
            .await
            .unwrap();
    }

    let change = SourceChange::Update {
        element: v3.clone(),
    };

    let result = query.process_source_change(change.clone()).await.unwrap();
    assert_eq!(result.len(), 1);

    let after = match result[0] {
        QueryPartEvaluationContext::Updating { ref after, .. } => after,
        _ => panic!("Expected Updating"),
    };

    assert_eq!(
        after,
        &variablemap!(
            "t" => v3.to_expression_variable(),
            "range" => VariableValue::List(vec![
                v1.to_expression_variable(),
                v2.to_expression_variable(),
            ])
        )
    );
}
