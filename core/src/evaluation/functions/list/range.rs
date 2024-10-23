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
pub struct Range {}

#[async_trait]
impl ScalarFunction for Range {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() < 2 || args.len() > 3 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match args.len() {
            2 => match (&args[0], &args[1]) {
                (VariableValue::Integer(start), VariableValue::Integer(end)) => {
                    let mut range = Vec::new();
                    let start = match start.as_i64() {
                        Some(i) => i,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    };
                    let end = match end.as_i64() {
                        Some(i) => i,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    };
                    for i in start..end + 1 {
                        range.push(VariableValue::Integer(i.into()));
                    }
                    Ok(VariableValue::List(range))
                }
                (VariableValue::Integer(_), _) => Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(1),
                }),
                _ => Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(0),
                }),
            },
            3 => match (&args[0], &args[1], &args[2]) {
                (
                    VariableValue::Integer(start),
                    VariableValue::Integer(end),
                    VariableValue::Integer(step),
                ) => {
                    let mut range = Vec::new();
                    let start = match start.as_i64() {
                        Some(i) => i,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    };
                    let end = match end.as_i64() {
                        Some(i) => i,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    };
                    let step = match step.as_i64() {
                        Some(i) => i,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    };
                    for i in (start..end + 1).step_by(step as usize) {
                        range.push(VariableValue::Integer(i.into()));
                    }
                    Ok(VariableValue::List(range))
                }
                (VariableValue::Integer(_), VariableValue::Integer(_), _) => Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(2),
                }),
                (_, VariableValue::Integer(_), VariableValue::Integer(_)) => Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(0),
                }),
                (VariableValue::Integer(_), _, VariableValue::Integer(_)) => Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(1),
                }),
                _ => Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(0),
                }),
            },
            _ => unreachable!(),
        }
    }
}
