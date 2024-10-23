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
use crate::interface::ResultKey;
use crate::interface::ResultOwner;
use crate::interface::{FutureQueue, PushType};
use async_trait::async_trait;
use chrono::Duration;
use drasi_query_ast::ast;

pub struct TrueFor {
    future_queue: Arc<dyn FutureQueue>,
    result_index: Arc<dyn ResultIndex>,
    expression_evaluator: Weak<ExpressionEvaluator>,
}

impl TrueFor {
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
impl ScalarFunction for TrueFor {
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

        let result_owner = ResultOwner::Function(expression.position_in_query);

        let anchor_element = match context.get_anchor_element() {
            Some(anchor) => anchor,
            None => return Ok(VariableValue::Null),
        };

        let anchor_ref = anchor_element.get_reference().clone();

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

        let duration = match &args[1] {
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
                    error: FunctionEvaluationError::InvalidArgument(1),
                })
            }
        };

        let group_signature = context.get_input_grouping_hash();

        let expression_evaluator = match self.expression_evaluator.upgrade() {
            Some(evaluator) => evaluator,
            None => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        let result_key = match context.get_output_grouping_key() {
            Some(group_expressions) => {
                let mut grouping_vals = Vec::new();
                for group_expression in group_expressions {
                    grouping_vals.push(
                        match expression_evaluator
                            .evaluate_expression(context, group_expression)
                            .await
                        {
                            Ok(val) => val,
                            Err(_e) => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidType {
                                        expected: "VariableValue".to_string(),
                                    },
                                })
                            }
                        },
                    );
                }
                ResultKey::GroupBy(Arc::new(grouping_vals))
            }
            None => ResultKey::InputHash(group_signature),
        };

        if !*condition {
            if let SideEffects::Apply = context.get_side_effects() {
                match self
                    .result_index
                    .set(result_key.clone(), result_owner, None)
                    .await
                {
                    Ok(()) => (),
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::IndexError(e),
                        })
                    }
                }

                match self
                    .future_queue
                    .remove(expression.position_in_query, group_signature)
                    .await
                {
                    Ok(()) => (),
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::IndexError(e),
                        })
                    }
                }
            }
            return Ok(VariableValue::Bool(*condition));
        }

        let due_time = match self.result_index.get(&result_key, &result_owner).await {
            Ok(Some(ValueAccumulator::TimeMarker {
                timestamp: since_timestamp,
            })) => {
                if let SideEffects::RevertForDelete = context.get_side_effects() {
                    match self
                        .result_index
                        .set(result_key.clone(), result_owner, None)
                        .await
                    {
                        Ok(()) => (),
                        Err(e) => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::IndexError(e),
                            })
                        }
                    }
                }

                since_timestamp + duration.num_milliseconds() as u64
            }
            Ok(None) => {
                if let SideEffects::Apply = context.get_side_effects() {
                    match self
                        .result_index
                        .set(
                            result_key.clone(),
                            result_owner,
                            Some(ValueAccumulator::TimeMarker {
                                timestamp: context.get_transaction_time(),
                            }),
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
                    }
                }

                context.get_transaction_time() + duration.num_milliseconds() as u64
            }
            Ok(_) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
            Err(e) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::IndexError(e),
                })
            }
        };

        if due_time <= context.get_realtime() {
            if let SideEffects::Apply = context.get_side_effects() {
                match self
                    .future_queue
                    .remove(expression.position_in_query, group_signature)
                    .await
                {
                    Ok(()) => (),
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::IndexError(e),
                        })
                    }
                }
            }
            return Ok(VariableValue::Bool(*condition));
        }

        if let SideEffects::Apply = context.get_side_effects() {
            match self
                .future_queue
                .push(
                    PushType::IfNotExists,
                    expression.position_in_query,
                    group_signature,
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
