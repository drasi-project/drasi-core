use std::sync::Arc;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

#[tokio::test]
async fn evaluate_addition() {
    let expr = "$param1 + $param2 + 1";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let clock = Arc::new(InstantQueryClock::new(0, 0));

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    {
        let context = ExpressionEvaluationContext::new(&variables, clock);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(6))
        );
    }
}

#[tokio::test]
async fn evaluate_subtraction() {
    let expr = "$param1 - $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let clock = Arc::new(InstantQueryClock::new(0, 0));

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    {
        let context = ExpressionEvaluationContext::new(&variables, clock);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(1))
        );
    }
}

#[tokio::test]
async fn evaluate_multiplication() {
    let expr = "$param1 * $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let clock = Arc::new(InstantQueryClock::new(0, 0));

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    {
        let context = ExpressionEvaluationContext::new(&variables, clock);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(6))
        );
    }
}

#[tokio::test]
async fn evaluate_division() {
    let expr = "$param1 / $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let clock = Arc::new(InstantQueryClock::new(0, 0));

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(10)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    {
        let context = ExpressionEvaluationContext::new(&variables, clock);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Float(Float::from(5))
        );
    }
}

#[tokio::test]
async fn evaluate_modulo() {
    let expr = "$param1 % $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let clock = Arc::new(InstantQueryClock::new(0, 0));

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(10)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context = ExpressionEvaluationContext::new(&variables, clock);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(1))
        );
    }
}

#[tokio::test]
async fn evaluate_exponent() {
    let expr = "$param1 ^ $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let clock = Arc::new(InstantQueryClock::new(0, 0));

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context = ExpressionEvaluationContext::new(&variables, clock);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(Integer::from(8))
        );
    }
}
