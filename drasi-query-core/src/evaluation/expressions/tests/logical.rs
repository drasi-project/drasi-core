use std::sync::Arc;

use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

#[tokio::test]
async fn evaluate_logical_predicate() {
    let expr = "$param1 = 1 AND ($param2 = 2 OR $param3 = 3)";
    let predicate = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert("param1".into(), VariableValue::Integer(Integer::from(1)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param3".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_predicate(&context, &predicate)
                .await
                .unwrap(),
            true
        );
    }

    variables.insert("param3".into(), VariableValue::Integer(Integer::from(4)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_predicate(&context, &predicate)
                .await
                .unwrap(),
            true
        );
    }

    variables.insert("param2".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_predicate(&context, &predicate)
                .await
                .unwrap(),
            false
        );
    }

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param3".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_predicate(&context, &predicate)
                .await
                .unwrap(),
            false
        );
    }
}

#[tokio::test]
async fn evaluate_not() {
    let expr = "NOT ($param1 = $param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::String(String::from("a")));
    variables.insert("param2".into(), VariableValue::String(String::from("a")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
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
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn evaluate_and() {
    let expr = "$param1 AND $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Bool(true));
    variables.insert("param2".into(), VariableValue::Bool(true));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }

    variables.insert("param1".into(), VariableValue::Bool(false));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }
}

#[tokio::test]
async fn evaluate_or() {
    let expr = "$param1 OR $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Bool(true));
    variables.insert("param2".into(), VariableValue::Bool(false));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }

    variables.insert("param1".into(), VariableValue::Bool(false));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }
}
