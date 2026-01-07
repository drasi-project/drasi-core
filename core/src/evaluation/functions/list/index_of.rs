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

use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

#[derive(Debug)]
pub struct IndexOf {}

#[async_trait]
impl ScalarFunction for IndexOf {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Null, _) | (_, VariableValue::Null) => Ok(VariableValue::Null),
            (VariableValue::List(l), value) => {
                for (i, item) in l.iter().enumerate() {
                    if item == value {
                        return Ok(VariableValue::Integer((i as i64).into()));
                    }
                }
                Ok(VariableValue::Integer((-1_i64).into()))
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::context::QueryVariables;
    use crate::evaluation::InstantQueryClock;
    use std::sync::Arc;

    fn get_func_expr() -> ast::FunctionExpression {
        ast::FunctionExpression {
            name: Arc::from("coll.indexOf"),
            args: vec![],
            position_in_query: 10,
        }
    }

    #[tokio::test]
    async fn test_index_of_found() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![
                VariableValue::String("a".to_string()),
                VariableValue::String("b".to_string()),
                VariableValue::String("c".to_string()),
                VariableValue::String("c".to_string()),
            ]),
            VariableValue::String("c".to_string()),
        ];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Integer(2.into()));
    }

    #[tokio::test]
    async fn test_index_of_first_element() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::Integer(2.into()),
                VariableValue::Integer(3.into()),
            ]),
            VariableValue::Integer(1.into()),
        ];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Integer(0.into()));
    }

    #[tokio::test]
    async fn test_index_of_not_found() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::String("b".to_string()),
                VariableValue::Bool(false),
            ]),
            VariableValue::Float(4.3.into()),
        ];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Integer((-1_i64).into()));
    }

    #[tokio::test]
    async fn test_index_of_empty_list() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![]),
            VariableValue::String("a".to_string()),
        ];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Integer((-1_i64).into()));
    }

    #[tokio::test]
    async fn test_index_of_null_list() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::Null, VariableValue::String("a".to_string())];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_index_of_null_value() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![VariableValue::Integer(1.into())]),
            VariableValue::Null,
        ];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_index_of_both_null() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::Null, VariableValue::Null];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_index_of_invalid_list_type() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::Integer(42.into()),
            VariableValue::String("a".to_string()),
        ];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgument(0)
            }
        ));
    }

    #[tokio::test]
    async fn test_index_of_wrong_arg_count() {
        let index_of = IndexOf {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::List(vec![])];
        let result = index_of.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        ));
    }
}
