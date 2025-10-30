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

use std::sync::Arc;

use crate::evaluation::context::SideEffects;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::{FunctionError, FunctionEvaluationError};
use crate::interface::{FutureQueue, PushType};
use async_trait::async_trait;
use chrono::NaiveTime;
use drasi_query_ast::ast;

pub struct TrueLater {
    future_queue: Arc<dyn FutureQueue>,
}

impl TrueLater {
    pub fn new(future_queue: Arc<dyn FutureQueue>) -> Self {
        TrueLater { future_queue }
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
#[async_trait]
impl ScalarFunction for TrueLater {
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

        let anchor_ref = match context.get_anchor_element() {
            Some(anchor) => anchor.get_reference().clone(),
            None => {
                return Ok(VariableValue::Null);
            }
        };

        let condition = match &args[0] {
            VariableValue::Bool(b) => b,
            VariableValue::Null => return Ok(VariableValue::Null),
            _ => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(0),
                })
            }
        };

        let due_time = match &args[1] {
            VariableValue::Date(d) => {
                d.and_time(NaiveTime::MIN).and_utc().timestamp_millis() as u64
            }
            VariableValue::LocalDateTime(d) => d.and_utc().timestamp_millis() as u64,
            VariableValue::ZonedDateTime(d) => d.datetime().timestamp_millis() as u64,
            VariableValue::Integer(n) => match n.as_u64() {
                Some(u) => u,
                None => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    })
                }
            },
            VariableValue::Null => return Ok(VariableValue::Null),
            _ => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidArgument(1),
                })
            }
        };

        let input_signature = context.get_input_grouping_hash();

        if due_time <= context.get_realtime() {
            return Ok(VariableValue::Bool(*condition));
        }

        if let SideEffects::Apply = context.get_side_effects() {
            match self
                .future_queue
                .push(
                    PushType::Overwrite,
                    expression.position_in_query,
                    input_signature,
                    &anchor_ref,
                    context.get_transaction_time(),
                    due_time,
                )
                .await
            {
                Ok(_) => (),
                Err(e) => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::IndexError(e),
                    })
                }
            }
        }

        Ok(VariableValue::Awaiting)
    }
}
