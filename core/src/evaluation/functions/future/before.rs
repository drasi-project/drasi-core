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

use std::collections::BTreeMap;
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
use crate::models::ElementValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

pub struct Before {
    result_index: Arc<dyn ResultIndex>,
    expression_evaluator: Weak<ExpressionEvaluator>,
}

impl Before {
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
impl ScalarFunction for Before {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() < 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let result_owner = ResultOwner::Function(expression.position_in_query);
        let value = &args[0];
        let default_value: ElementValue = {
            if args.len() > 1 { &args[1] } else { &VariableValue::Null }
        }.try_into().map_err(|_e| FunctionError {
            function_name: expression.name.to_string(),
            error: FunctionEvaluationError::InvalidType {
                expected: "ElementValue".to_string(),
            },
        })?;
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

        let current_value: ElementValue = value.try_into().map_err(|_e| FunctionError {
            function_name: expression.name.to_string(),
            error: FunctionEvaluationError::InvalidType {
                expected: "ElementValue".to_string(),
            },
        })?;

        let prev_values = match self.result_index.get(&result_key, &result_owner).await {
            Ok(v) => match v {
                Some(v) => match v {
                    ValueAccumulator::Map(m) => (m.get("0").cloned().unwrap_or_default(), m.get("1").cloned().unwrap_or_default()),
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidType {
                                expected: "ElementValue".to_string(),
                            },
                        })
                    }
                },
                None => (default_value.clone(), default_value.clone()),
            },
            Err(e) => {
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::IndexError(e),
                })
            }
        };

        println!(
            "Before function called with current value {:?} and previous value {:?}",
            current_value, prev_values
        );

        match context.get_side_effects() {
            SideEffects::Apply => {
                println!("Storing current value {:?} for key {:?}", current_value, result_key);
                match self
                    .result_index
                    .set(
                        result_key.clone(),
                        result_owner,
                        Some(ValueAccumulator::Map(BTreeMap::from([("0".to_string(), current_value), ("1".to_string(), prev_values.0.clone())]).into())),
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
                Ok((&prev_values.0).into())
            },
            SideEffects::Snapshot => Ok((&prev_values.1).into()),
            SideEffects::RevertForUpdate => Ok((&prev_values.1).into()),
            SideEffects::RevertForDelete => Ok((&prev_values.1).into())
        }
    }
}
