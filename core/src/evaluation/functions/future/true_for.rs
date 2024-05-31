use std::sync::Arc;
use std::sync::Weak;

use crate::evaluation::context::SideEffects;
use crate::evaluation::functions::aggregation::ValueAccumulator;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::EvaluationError;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::ExpressionEvaluator;
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
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 2 {
            return Err(EvaluationError::InvalidArgumentCount(
                expression.name.to_string(),
            ));
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
            _ => return Err(EvaluationError::InvalidType),
        };

        let duration = match &args[1] {
            VariableValue::Duration(d) => d.duration().clone(),
            VariableValue::Integer(n) => match n.as_i64() {
                Some(ms) => Duration::milliseconds(ms),
                None => return Err(EvaluationError::InvalidType),
            },
            VariableValue::Null => return Ok(VariableValue::Null),
            _ => return Err(EvaluationError::InvalidType),
        };

        let group_signature = context.get_input_grouping_hash();

        let expression_evaluator = match self.expression_evaluator.upgrade() {
            Some(evaluator) => evaluator,
            None => return Err(EvaluationError::InvalidState),
        };

        let result_key = match context.get_output_grouping_key() {
            Some(group_expressions) => {
                let mut grouping_vals = Vec::new();
                for group_expression in group_expressions {
                    grouping_vals.push(
                        expression_evaluator
                            .evaluate_expression(context, group_expression)
                            .await?,
                    );
                }
                ResultKey::GroupBy(Arc::new(grouping_vals))
            }
            None => ResultKey::InputHash(group_signature),
        };

        if !*condition {
            if let SideEffects::Apply = context.get_side_effects() {
                self.result_index
                    .set(result_key.clone(), result_owner, None)
                    .await?;

                self.future_queue
                    .remove(expression.position_in_query, group_signature)
                    .await?;
            }
            return Ok(VariableValue::Bool(*condition));
        }

        let due_time = match self.result_index.get(&result_key, &result_owner).await? {
            Some(ValueAccumulator::TimeMarker {
                timestamp: since_timestamp,
            }) => {
                if let SideEffects::RevertForDelete = context.get_side_effects() {
                    self.result_index
                        .set(result_key.clone(), result_owner, None)
                        .await?;
                }

                since_timestamp + duration.num_milliseconds() as u64
            }
            None => {
                if let SideEffects::Apply = context.get_side_effects() {
                    self.result_index
                        .set(
                            result_key.clone(),
                            result_owner,
                            Some(ValueAccumulator::TimeMarker {
                                timestamp: context.get_transaction_time(),
                            }),
                        )
                        .await?;
                }

                context.get_transaction_time() + duration.num_milliseconds() as u64
            }
            _ => return Err(EvaluationError::InvalidType),
        };

        if due_time <= context.get_realtime() {
            if let SideEffects::Apply = context.get_side_effects() {
                self.future_queue
                    .remove(expression.position_in_query, group_signature)
                    .await?;
            }
            return Ok(VariableValue::Bool(*condition));
        }

        if let SideEffects::Apply = context.get_side_effects() {
            self.future_queue
                .push(
                    PushType::IfNotExists,
                    expression.position_in_query,
                    group_signature,
                    &anchor_ref,
                    context.get_transaction_time(),
                    due_time,
                )
                .await?;
        }

        Ok(VariableValue::Awaiting)
    }
}
