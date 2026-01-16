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
use std::collections::HashSet;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

#[derive(Debug)]
pub struct Distinct {}

#[async_trait]
impl ScalarFunction for Distinct {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::List(l) => {
                let mut result = Vec::with_capacity(l.len());
                let mut seen: HashSet<VariableValue> = HashSet::with_capacity(l.len());

                for value in l {
                    if seen.insert(value.clone()) {
                        result.push(value.clone());
                    }
                }
                Ok(VariableValue::List(result))
            }
            VariableValue::Null => Ok(VariableValue::Null),
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
            name: Arc::from("coll.distinct"),
            args: vec![],
            position_in_query: 10,
        }
    }

    #[tokio::test]
    async fn test_distinct_integers_with_duplicates() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::List(vec![
            VariableValue::Integer(1.into()),
            VariableValue::Integer(3.into()),
            VariableValue::Integer(2.into()),
            VariableValue::Integer(4.into()),
            VariableValue::Integer(2.into()),
            VariableValue::Integer(3.into()),
            VariableValue::Integer(1.into()),
        ])];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert_eq!(
            result.unwrap(),
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::Integer(3.into()),
                VariableValue::Integer(2.into()),
                VariableValue::Integer(4.into()),
            ])
        );
    }

    #[tokio::test]
    async fn test_distinct_mixed_types() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::List(vec![
            VariableValue::Integer(1.into()),
            VariableValue::Bool(true),
            VariableValue::Bool(true),
            VariableValue::Null,
            VariableValue::String("a".to_string()),
            VariableValue::Bool(false),
            VariableValue::Bool(true),
            VariableValue::Integer(1.into()),
            VariableValue::Null,
        ])];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert_eq!(
            result.unwrap(),
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::Bool(true),
                VariableValue::Null,
                VariableValue::String("a".to_string()),
                VariableValue::Bool(false),
            ])
        );
    }

    #[tokio::test]
    async fn test_distinct_empty_list() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::List(vec![])];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::List(vec![]));
    }

    #[tokio::test]
    async fn test_distinct_no_duplicates() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::List(vec![
            VariableValue::Integer(1.into()),
            VariableValue::Integer(2.into()),
            VariableValue::Integer(3.into()),
        ])];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert_eq!(
            result.unwrap(),
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::Integer(2.into()),
                VariableValue::Integer(3.into()),
            ])
        );
    }

    #[tokio::test]
    async fn test_distinct_null() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::Null];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_distinct_invalid_type() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![VariableValue::Integer(42.into())];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgument(0)
            }
        ));
    }

    #[tokio::test]
    async fn test_distinct_wrong_arg_count() {
        let distinct = Distinct {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![];
        let result = distinct.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        ));
    }
}
