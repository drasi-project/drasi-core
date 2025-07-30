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

use drasi_core::evaluation::functions::ast;
use drasi_core::evaluation::functions::async_trait;
use drasi_core::evaluation::functions::{ScalarFunction, ToBoolean, ToFloat, ToInteger, ToString};
use drasi_core::evaluation::variable_value::VariableValue;
use drasi_core::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

#[derive(Debug)]
pub struct Cast {}

#[async_trait]
impl ScalarFunction for Cast {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let value = &args[0];
        let target_type = match &args[1] {
            VariableValue::String(s) => s.to_uppercase(),
            _ => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(1),
                })
            }
        };

        match target_type.as_str() {
            "STRING" => {
                let to_string = ToString {};
                to_string
                    .call(context, expression, vec![value.clone()])
                    .await
            }
            "INTEGER" | "INT" => {
                let to_integer = ToInteger {};
                to_integer
                    .call(context, expression, vec![value.clone()])
                    .await
            }
            "FLOAT" => {
                let to_float = ToFloat {};
                to_float
                    .call(context, expression, vec![value.clone()])
                    .await
            }
            "BOOLEAN" | "BOOL" => {
                let to_boolean = ToBoolean {};
                to_boolean
                    .call(context, expression, vec![value.clone()])
                    .await
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidType {
                    expected: "STRING, INTEGER/INT, FLOAT, or BOOLEAN/BOOL".to_string(),
                },
            }),
        }
    }
}
