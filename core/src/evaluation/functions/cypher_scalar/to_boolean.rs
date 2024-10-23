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
pub struct ToBoolean {}

#[async_trait]
impl ScalarFunction for ToBoolean {
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
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(i) => Ok(VariableValue::Bool(match i.as_i64() {
                Some(i) => i != 0,
                None => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    })
                }
            })),
            VariableValue::String(s) => {
                let s = s.to_lowercase();
                if (s == "true") || (s == "false") {
                    Ok(VariableValue::Bool(s == "true"))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            VariableValue::Bool(b) => Ok(VariableValue::Bool(*b)),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct ToBooleanOrNull {}

#[async_trait]
impl ScalarFunction for ToBooleanOrNull {
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
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(i) => Ok(VariableValue::Bool(match i.as_i64() {
                Some(i) => i != 0,
                None => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    })
                }
            })),
            VariableValue::String(s) => {
                if (s == "true") || (s == "false") {
                    Ok(VariableValue::Bool(s == "true"))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            VariableValue::Bool(b) => Ok(VariableValue::Bool(*b)),
            _ => Ok(VariableValue::Null),
        }
    }
}
