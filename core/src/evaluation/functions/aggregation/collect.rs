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

use std::{collections::HashMap, fmt::Debug, sync::Arc};
use std::collections::hash_map::Entry;

use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::{
    evaluation::{
        variable_value::VariableValue, ExpressionEvaluationContext, FunctionError,
        FunctionEvaluationError,
    },
    interface::ResultIndex,
};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Collect {}

fn build_list_from_counts(counts: &HashMap<VariableValue, usize>) -> Vec<VariableValue> {
    let mut result_list = Vec::new();
    for (value, count) in counts {
        for _ in 0..*count {
            result_list.push(value.clone());
        }
    }
    // Order is not guaranteed by HashMap iteration, which matches Cypher's collect behavior.
    result_list
}


#[async_trait]
impl AggregatingFunction for Collect {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::CollectCounts {
            counts: HashMap::new(),
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
                function_name: "collect".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let counts = match accumulator {
            Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => counts,
            _ => {
                return Err(FunctionError {
                    function_name: "collect".to_string(),
                    error: FunctionEvaluationError::CorruptData, // Or InvalidAccumulatorType
                })
            }
        };

        // Ignore null values
        if let VariableValue::Null = &args[0] {
             // Return the current state without modification
             let result_list = build_list_from_counts(counts);
             return Ok(VariableValue::List(result_list));
        }

        *counts.entry(args[0].clone()).or_insert(0) += 1;

        let result_list = build_list_from_counts(counts);
        Ok(VariableValue::List(result_list))
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "collect".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        // Match against the new CollectCounts variant
        let counts = match accumulator {
            Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => counts,
            _ => {
                return Err(FunctionError {
                    function_name: "collect".to_string(),
                    error: FunctionEvaluationError::CorruptData, // Or InvalidAccumulatorType
                })
            }
        };

        // Ignore null values 
        if let VariableValue::Null = &args[0] {
            // Return the current state without modification
            let result_list = build_list_from_counts(counts);
            return Ok(VariableValue::List(result_list));
        }

        match counts.entry(args[0].clone()) {
            Entry::Occupied(mut entry) => {
                let current_count = entry.get_mut();
                if *current_count > 0 {
                     *current_count -= 1;
                }
                // Remove the entry if the count becomes zero
                if *current_count == 0 {
                    entry.remove();
                }
            }
            Entry::Vacant(_) => {
                //TODO: This case (reverting a value not present) might indicate an inconsistency,
                // but for robustness, we can ignore it or log a warning.
            }
        }

        let result_list = build_list_from_counts(counts);
        Ok(VariableValue::List(result_list))
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: Vec<VariableValue>, // Args usually not needed for snapshot
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
         let counts = match accumulator {
            Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => counts,
            _ => {
                return Err(FunctionError {
                    function_name: "collect".to_string(),
                    error: FunctionEvaluationError::CorruptData, // Or InvalidAccumulatorType
                })
            }
        };

        let result_list = build_list_from_counts(counts);
        Ok(VariableValue::List(result_list))
    }
}

impl Debug for Collect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Collect")
    }
}