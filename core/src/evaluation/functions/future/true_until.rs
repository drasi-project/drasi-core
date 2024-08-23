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

pub struct TrueUntil {
    future_queue: Arc<dyn FutureQueue>,
}

impl TrueUntil {
    pub fn new(future_queue: Arc<dyn FutureQueue>) -> Self {
        TrueUntil { future_queue }
    }
}

#[async_trait]
impl ScalarFunction for TrueUntil {
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
            None => return Ok(VariableValue::Null),
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

        let group_signature = context.get_input_grouping_hash();

        if !*condition {
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
                };
            }
            return Ok(VariableValue::Bool(*condition));
        }

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
                };
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
            };
        }

        Ok(VariableValue::Awaiting)
    }
}
