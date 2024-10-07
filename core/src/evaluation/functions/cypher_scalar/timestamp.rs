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
pub struct Timestamp {}

#[async_trait]
impl ScalarFunction for Timestamp {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if !args.is_empty() {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let now = std::time::SystemTime::now();
        let since_epoch = match now.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => {
                if d.as_millis() > i64::MAX as u128 {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    });
                } else {
                    d.as_millis() as i64
                }
            }
            Err(_e) => {
                // This should never happen, since duration_since will ony return an error if the time is before the UNIX_EPOCH
                // return a zero duration this case
                0
            }
        };
        Ok(VariableValue::Integer((since_epoch).into()))
    }
}
