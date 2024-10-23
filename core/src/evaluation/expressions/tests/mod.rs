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

use serde_json::json;

use crate::{
    evaluation::{context::QueryVariables, variable_value::VariableValue, InstantQueryClock},
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

use super::*;

mod arithmetic;
mod comparison;
mod cypher_scalar;
mod date;
mod datetime;
mod duration;
mod list_construction;
mod list_functions;
mod list_indexing;
mod list_reduce;
mod logical;
mod numeric;
mod string;
mod time;

#[tokio::test]
async fn evaluate_property_access() {
    let expr = "o.Name";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("o".into(), VariableValue::from(json!({"Name" : "foo"})));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String(String::from("foo"))
        );
    }
}

#[tokio::test]
async fn evaluate_case() {
    let expr = "CASE
    WHEN $param1 = 'a' THEN 1
    WHEN $param1 = 'b' THEN 2
    ELSE 3
  END";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::String(String::from("a")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(1))
        );
    }

    variables.insert("param1".into(), VariableValue::String(String::from("b")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(2))
        );
    }

    variables.insert("param1".into(), VariableValue::String(String::from("z")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(3))
        );
    }
}

#[tokio::test]
async fn evaluate_case_match() {
    let expr = "CASE $param1
    WHEN 'a' THEN 1
    WHEN 'b' THEN 2
    ELSE 3
  END";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::String(String::from("a")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(1))
        );
    }

    variables.insert("param1".into(), VariableValue::String(String::from("b")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(2))
        );
    }

    variables.insert("param1".into(), VariableValue::String(String::from("z")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(3))
        );
    }
}
