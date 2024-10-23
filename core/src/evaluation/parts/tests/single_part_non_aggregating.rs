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

use super::process_solution;

use crate::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        parts::tests::build_query, variable_value::VariableValue, ExpressionEvaluator,
        QueryPartEvaluator,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

use serde_json::json;

#[tokio::test]
async fn add_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 = 1 RETURN a");

    let node1 = VariableValue::from(json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    }));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()]
        }]
    );
}

#[tokio::test]
async fn update_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 < 10 RETURN a");

    let node1 = VariableValue::from(json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    }));

    let node2 = VariableValue::from(json!({
      "id": 1,
      "Value1": 2,
      "Name": "bar"
    }));

    let node3 = VariableValue::from(json!({
      "id": 1,
      "Value1": 10,
      "Name": "bar"
    }));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node2.clone()]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node2.clone()],
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
        }]
    );
}

#[tokio::test]
async fn remove_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 = 1 RETURN a");

    let node1 = VariableValue::from(json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    }));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()]
        }]
    );
}
