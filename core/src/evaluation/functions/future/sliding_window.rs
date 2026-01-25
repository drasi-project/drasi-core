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

use std::result;
use std::sync::Arc;
use std::sync::Weak;

use crate::evaluation::context::SideEffects;
use crate::evaluation::functions::LazyScalarFunction;
use crate::evaluation::functions::ValueAccumulator;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::ExpressionEvaluator;
use crate::evaluation::{FunctionError, FunctionEvaluationError};
use crate::interface::ResultIndex;
use crate::interface::ResultKey;
use crate::interface::ResultOwner;
use crate::interface::{FutureQueue, PushType};
use async_trait::async_trait;
use chrono::Duration;
use drasi_query_ast::ast;

pub struct SlidingWindow {
    future_queue: Arc<dyn FutureQueue>,
    result_index: Arc<dyn ResultIndex>,
    expression_evaluator: Weak<ExpressionEvaluator>,
}

impl SlidingWindow {
    pub fn new(
        future_queue: Arc<dyn FutureQueue>,
        result_index: Arc<dyn ResultIndex>,
        expression_evaluator: Weak<ExpressionEvaluator>,
    ) -> Self {
        Self {
            future_queue,
            result_index,
            expression_evaluator,
        }
    }
}

#[async_trait]
impl LazyScalarFunction for SlidingWindow {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: &Vec<ast::Expression>,
    ) -> Result<VariableValue, FunctionError> {
        let result_owner = ResultOwner::Function(expression.position_in_query);

        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let expression_evaluator = match self.expression_evaluator.upgrade() {
            Some(evaluator) => evaluator,
            None => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        let window_arg = match expression_evaluator
            .evaluate_expression(context, &args[0])
            .await
        {
            Ok(value) => value,
            Err(e) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::EvaluationError(Box::new(e)),
                })
            }
        };

        let expression_arg = &args[1];

        let window_size = match &window_arg {
            VariableValue::Duration(d) => *d.duration(),
            VariableValue::Integer(n) => match n.as_i64() {
                Some(ms) => Duration::milliseconds(ms),
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
                    error: FunctionEvaluationError::InvalidArgument(0),
                })
            }
        };

        let input_signature = context.get_input_grouping_hash();
        let due_time = context.get_transaction_time() + (window_size.num_milliseconds() as u64);
        let expired = context.get_realtime() >= due_time;

        if let Some(anchor_element) = context.get_anchor_element() {
            let result_key = ResultKey::Element(anchor_element.get_reference().clone());

            #[allow(clippy::single_match)]
            match context.get_side_effects() {
                SideEffects::Apply => {
                    if expired {
                        match self
                            .result_index
                            .set(
                                result_key,
                                result_owner.clone(),
                                Some(ValueAccumulator::Signature(1)),
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

                        let mut new_context = context.clone();
                        new_context.set_side_effects(SideEffects::Snapshot);
                        return match expression_evaluator
                            .evaluate_expression(&new_context, expression_arg)
                            .await
                        {
                            Ok(value) => Ok(value),
                            Err(e) => Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::EvaluationError(Box::new(e)),
                            }),
                        };
                    }
                    let anchor_ref = anchor_element.get_reference().clone();
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
                    };
                }
                SideEffects::RevertForUpdate | SideEffects::RevertForDelete => {
                    match self.result_index.get(&result_key, &result_owner).await {
                        Ok(Some(ValueAccumulator::Signature(_))) => {
                            match self.result_index.set(result_key, result_owner, None).await {
                                Ok(()) => (),
                                Err(e) => {
                                    return Err(FunctionError {
                                        function_name: expression.name.to_string(),
                                        error: FunctionEvaluationError::IndexError(e),
                                    })
                                }
                            };
                            return match expression_evaluator
                                .evaluate_expression(context, expression_arg)
                                .await
                            {
                                Ok(value) => Ok(value),
                                Err(e) => Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::EvaluationError(Box::new(e)),
                                }),
                            };
                        }
                        _ => {}
                    }
                }
                _ => (),
            }
        }

        match expression_evaluator
            .evaluate_expression(context, expression_arg)
            .await
        {
            Ok(value) => Ok(value),
            Err(e) => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::EvaluationError(Box::new(e)),
            }),
        }
    }
}
