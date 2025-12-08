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

use std::{fmt::Debug, sync::Arc};

use crate::{
    evaluation::{FunctionError, FunctionEvaluationError},
    interface::ResultIndex,
};

use async_trait::async_trait;

use drasi_query_ast::ast;

use crate::evaluation::{variable_value::VariableValue, ExpressionEvaluationContext};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Collect {}

#[async_trait]
impl AggregatingFunction for Collect {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::List {
            values: Vec::new(),
        })
    }

    fn accumulator_is_lazy(&self) -> bool {
        false
    }

    async fn apply(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Collect".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let values = match accumulator {
            Accumulator::Value(ValueAccumulator::List { values }) => values,
            _ => {
                return Err(FunctionError {
                    function_name: "Collect".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Null => Ok(VariableValue::List(values.clone())),
            value => {
                values.push(value.clone());
                Ok(VariableValue::List(values.clone()))
            }
        }
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Collect".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let values = match accumulator {
            Accumulator::Value(ValueAccumulator::List { values }) => values,
            _ => {
                return Err(FunctionError {
                    function_name: "Collect".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Null => Ok(VariableValue::List(values.clone())),
            value => {
                // Find and remove the first occurrence of the value
                if let Some(pos) = values.iter().position(|v| v == value) {
                    values.remove(pos);
                }
                Ok(VariableValue::List(values.clone()))
            }
        }
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        let values = match accumulator {
            Accumulator::Value(ValueAccumulator::List { values }) => values,
            _ => {
                return Err(FunctionError {
                    function_name: "Collect".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        Ok(VariableValue::List(values.clone()))
    }
}

impl Debug for Collect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Collect")
    }
}
