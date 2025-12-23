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
pub struct Insert {}

#[async_trait]
impl ScalarFunction for Insert {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 3 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Null, _) | (_, VariableValue::Null) => Ok(VariableValue::Null),
            (VariableValue::List(l), VariableValue::Integer(idx)) => {
                let index = match idx.as_i64() {
                    Some(i) => i,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(1),
                        });
                    }
                };
                if index < 0 || index as usize > l.len() {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(1),
                    });
                }
                let mut result = l.clone();
                result.insert(index as usize, args[2].clone());
                Ok(VariableValue::List(result))
            }
            (VariableValue::List(_), _) => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(1),
            }),
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
            name: Arc::from("coll.insert"),
            args: vec![],
            position_in_query: 10,
        }
    }

    #[tokio::test]
    async fn test_insert_middle() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![
                VariableValue::Bool(true),
                VariableValue::String("a".to_string()),
                VariableValue::Integer(1.into()),
                VariableValue::Float(5.4.into()),
            ]),
            VariableValue::Integer(1.into()),
            VariableValue::Bool(false),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert_eq!(
            result.unwrap(),
            VariableValue::List(vec![
                VariableValue::Bool(true),
                VariableValue::Bool(false),
                VariableValue::String("a".to_string()),
                VariableValue::Integer(1.into()),
                VariableValue::Float(5.4.into()),
            ])
        );
    }

    #[tokio::test]
    async fn test_insert_at_start() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::Integer(2.into()),
            ]),
            VariableValue::Integer(0.into()),
            VariableValue::Integer(0.into()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert_eq!(
            result.unwrap(),
            VariableValue::List(vec![
                VariableValue::Integer(0.into()),
                VariableValue::Integer(1.into()),
                VariableValue::Integer(2.into()),
            ])
        );
    }

    #[tokio::test]
    async fn test_insert_at_end() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![
                VariableValue::Integer(1.into()),
                VariableValue::Integer(2.into()),
            ]),
            VariableValue::Integer(2.into()),
            VariableValue::Integer(3.into()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
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
    async fn test_insert_into_empty_list() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![]),
            VariableValue::Integer(0.into()),
            VariableValue::String("first".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert_eq!(
            result.unwrap(),
            VariableValue::List(vec![VariableValue::String("first".to_string()),])
        );
    }

    #[tokio::test]
    async fn test_insert_null_list() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::Null,
            VariableValue::Integer(1.into()),
            VariableValue::String("value".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_insert_null_index() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![VariableValue::Integer(1.into())]),
            VariableValue::Null,
            VariableValue::String("value".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_insert_both_null() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::Null,
            VariableValue::Null,
            VariableValue::String("value".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert_eq!(result.unwrap(), VariableValue::Null);
    }

    #[tokio::test]
    async fn test_insert_negative_index() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![VariableValue::Integer(1.into())]),
            VariableValue::Integer((-1_i64).into()),
            VariableValue::String("value".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgument(1)
            }
        ));
    }

    #[tokio::test]
    async fn test_insert_index_too_large() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![VariableValue::Integer(1.into())]),
            VariableValue::Integer(5.into()),
            VariableValue::String("value".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgument(1)
            }
        ));
    }

    #[tokio::test]
    async fn test_insert_invalid_list_type() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::Integer(42.into()),
            VariableValue::Integer(0.into()),
            VariableValue::String("value".to_string()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgument(0)
            }
        ));
    }

    #[tokio::test]
    async fn test_insert_wrong_arg_count() {
        let insert = Insert {};
        let binding = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

        let args = vec![
            VariableValue::List(vec![]),
            VariableValue::Integer(0.into()),
        ];
        let result = insert.call(&context, &get_func_expr(), args).await;
        assert!(matches!(
            result.unwrap_err(),
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        ));
    }
}
