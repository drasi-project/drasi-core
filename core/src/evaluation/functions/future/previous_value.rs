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
use std::sync::Weak;

use crate::evaluation::context::SideEffects;
use crate::evaluation::functions::aggregation::ValueAccumulator;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::ExpressionEvaluator;
use crate::evaluation::{FunctionError, FunctionEvaluationError};
use crate::interface::ResultIndex;
use crate::interface::ResultOwner;
use async_trait::async_trait;
use drasi_query_ast::ast;

use super::state_codec::{self, StateCodecError};

pub struct PreviousValue {
    result_index: Arc<dyn ResultIndex>,
    expression_evaluator: Weak<ExpressionEvaluator>,
}

impl PreviousValue {
    pub fn new(
        result_index: Arc<dyn ResultIndex>,
        expression_evaluator: Weak<ExpressionEvaluator>,
    ) -> Self {
        Self {
            result_index,
            expression_evaluator,
        }
    }
}

#[async_trait]
impl ScalarFunction for PreviousValue {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.is_empty() {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let codec_error = |error: StateCodecError| FunctionError {
            function_name: expression.name.to_string(),
            error: error.into(),
        };

        let result_owner = ResultOwner::Function(expression.position_in_query);
        let value = &args[0];
        let default_value = state_codec::encode(if args.len() > 1 {
            &args[1]
        } else {
            &VariableValue::Null
        })
        .map_err(codec_error)?;

        let expression_evaluator = match self.expression_evaluator.upgrade() {
            Some(evaluator) => evaluator,
            None => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        let result_key = match expression_evaluator
            .resolve_context_result_key(context)
            .await
        {
            Ok(key) => key,
            Err(e) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::EvaluationError(Box::new(e)),
                })
            }
        };

        let current_value = state_codec::encode(value).map_err(codec_error)?;

        let prev_values = match self.result_index.get(&result_key, &result_owner).await {
            Ok(v) => match v {
                Some(v) => match v {
                    ValueAccumulator::Map(m) => state_codec::read_state(&m).map_err(codec_error)?,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidType {
                                expected: "ElementValue".to_string(),
                            },
                        })
                    }
                },
                None => [default_value.clone(), default_value],
            },
            Err(e) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::IndexError(e),
                })
            }
        };
        let [last_value, value_before_last] = prev_values;

        match context.get_side_effects() {
            SideEffects::Apply => {
                let result = state_codec::decode(&last_value).map_err(codec_error)?;
                match self
                    .result_index
                    .set(
                        result_key.clone(),
                        result_owner,
                        Some(state_codec::write_state(current_value, last_value)),
                    )
                    .await
                {
                    Ok(()) => (),
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::IndexError(e),
                        })
                    }
                };
                Ok(result)
            }
            SideEffects::Snapshot | SideEffects::RevertForUpdate | SideEffects::RevertForDelete => {
                state_codec::decode(&value_before_last).map_err(codec_error)
            }
        }
    }
}
