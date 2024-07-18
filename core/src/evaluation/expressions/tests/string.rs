use std::sync::Arc;

use crate::evaluation::variable_value::VariableValue;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;


#[tokio::test]
async fn evaluate_left() {
    let expr = "left('drasi', 3)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("dra".to_string())
        );
    }

    let expr = "left(NULL, 10)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}

#[tokio::test]
async fn evaluate_ltrim() {
    let expr = "ltrim('   drasi   ')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("drasi   ".to_string())
        );
    }

    let expr = "ltrim(   NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}

#[tokio::test]
async fn evaluate_replace() {
    let expr = "replace('hello', 'l', 'x')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("hexxo".to_string())
        );
    }

    let expr = "replace('-reactive-graph is ...? reactive-graph can xxxx', 'reactive-graph', 'drasi')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("-drasi is ...? drasi can xxxx".to_string())
        );
    }

    let expr = "replace('drasi', 'e', 't')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("drasi".to_string())
        );
    }

    let expr = "replace(NULL, 'e', 't')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}


#[tokio::test]
async fn evaluate_reverse() {
    let expr = "reverse('drasi')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("isard".to_string())
        );
    }

    let expr = "reverse(  NULL )";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}

#[tokio::test]
async fn evaluate_right() {
    let expr = "right('drasi', 3)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("asi".to_string())
        );
    }

    let expr = "right(null, 10)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}

#[tokio::test]
async fn evaluate_rtrim() {
    let expr = "rtrim('   drasi   ')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("   drasi".to_string())
        );
    }

    let expr = "rtrim(   NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}


#[tokio::test]
async fn evaluate_split() {
    let expr = "split('hello,world', ',')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::List(vec![
                VariableValue::String("hello".to_string()),
                VariableValue::String("world".to_string())
            ])
        );
    }

    let expr = "split('hello||world|| this is a test|| reactive-graph|| kubectl', '||')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::List(vec![
                VariableValue::String("hello".to_string()),
                VariableValue::String("world".to_string()),
                VariableValue::String(" this is a test".to_string()),
                VariableValue::String(" reactive-graph".to_string()),
                VariableValue::String(" kubectl".to_string())
            ])
        );
    }

    let expr = "split('hello||world|| this is&&a test||reactive-&&-graph|| kubectl', '||')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::List(vec![
                VariableValue::String("hello".to_string()),
                VariableValue::String("world".to_string()),
                VariableValue::String(" this is&&a test".to_string()),
                VariableValue::String("reactive-&&-graph".to_string()),
                VariableValue::String(" kubectl".to_string())
            ])
        );
    }

    let expr = "split(NULL, '||')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }

    let expr = "split('hello||world', NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }
}

#[tokio::test]
async fn evaluate_substring() {
    let expr = "substring('drasi', 2, 3)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("asi".to_string())
        );
    }

    let expr = "substring(NULL, 2, 3)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Null
        );
    }


    let expr = "substring('drasi', 2, 0)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::String("".to_string())
            );
    }
}


#[tokio::test]
async fn evaluate_to_lower() {
    let expr = "toLower('Drasi')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::String("drasi".to_string())
            );
    }

    let expr = "toLower(NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::Null
            );
    }
}


#[tokio::test]
async fn test_to_upper() {
    let expr = "toUpper('Drasi')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::String("DRASI".to_string())
            );
    }

    let expr = "toUpper(NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::Null
            );
    }
}

#[tokio::test]
async fn test_to_string() {
    let expr = "toString(123)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::String("123".to_string())
            );
    }

    let expr = "toString(NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::Null
            );
    }

    let expr = "toString(TRUE   )";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::String("true".to_string())
            );
    }
}


#[tokio::test]
async fn test_trim() {
    let expr = "trim('   drasi   ')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::String("drasi".to_string())
            );
    }

    let expr = "trim(   NULL)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expr)
                    .await
                    .unwrap(),
                VariableValue::Null
            );
    }
}