// Copyright 2026 The Drasi Authors.
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

use async_trait::async_trait;
use drasi_query_ast::ast::FunctionExpression;

use crate::{
    evaluation::{
        functions::{list, Accumulator, AggregatingFunction, ValueAccumulator},
        variable_value::VariableValue,
        ExpressionEvaluationContext, FunctionError, FunctionEvaluationError,
    },
    interface::ResultIndex,
    models::ElementValue,
};

/// Collect aggregation function that collects all values into a list
pub struct CollectList {}

#[async_trait]
impl AggregatingFunction for CollectList {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: std::sync::Arc<dyn ResultIndex>,
    ) -> Accumulator {
        // Initialize with an empty list
        Accumulator::Value(ValueAccumulator::Value(ElementValue::List(vec![])))
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
                function_name: "CollectList".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let list = match accumulator {
            Accumulator::Value(ValueAccumulator::Value(ElementValue::List(list))) => list,
            _ => {
                return Err(FunctionError {
                    function_name: "CollectList".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        // Get the ElementValue from the VariableValue
        let value = if args[0].is_null() {
            Some(ElementValue::Null)
        } else {
            (&args[0]).try_into().ok()
        };

        // Add the ElementValue into the list
        if let Some(v) = value {
            list.push(v);
        }

        // Return current list as VariableValue
        Ok((&ElementValue::List(list.clone())).into())
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "CollectList".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let list = match accumulator {
            Accumulator::Value(ValueAccumulator::Value(ElementValue::List(list))) => list,
            _ => {
                return Err(FunctionError {
                    function_name: "CollectList".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        let value = if args[0].is_null() {
            Some(ElementValue::Null)
        } else {
            (&args[0]).try_into().ok()
        };

        if let Some(elem_value) = value {
            // Find and remove the first matching value
            if let Some(pos) = list.iter().position(|x| x == &elem_value) {
                list.remove(pos);
            }
        }

        // Return current list as VariableValue
        Ok((&ElementValue::List(list.clone())).into())
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        let snaphot = match accumulator {
            Accumulator::Value(ValueAccumulator::Value(ElementValue::List(list))) => list,
            _ => {
                return Err(FunctionError {
                    function_name: "CollectList".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };
        Ok((&ElementValue::List(snaphot.clone())).into())
    }
}

impl Debug for CollectList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CollectList")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        evaluation::{
            context::QueryVariables, functions::CollectList, ExpressionEvaluationContext,
            InstantQueryClock,
        },
        in_memory_index::in_memory_result_index::InMemoryResultIndex,
    };
    use drasi_query_ast::ast;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_collect_basic() {
        let collect_list = CollectList {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect_list".into(),
            args: vec![],
            position_in_query: 10,
        };

        // Initialize accumulator
        let mut accumulator =
            collect_list.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply some values
        let val1 = VariableValue::String("hello".into());
        let val2 = VariableValue::Integer(42.into());
        let val3 = VariableValue::String("world".into());

        let _ = collect_list
            .apply(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect_list
            .apply(&context, vec![val2.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect_list
            .apply(&context, vec![val3.clone()], &mut accumulator)
            .await
            .unwrap();

        // Snapshot should return all values
        let result = collect_list
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 3);
            assert_eq!(list[0], val1);
            assert_eq!(list[1], val2);
            assert_eq!(list[2], val3);
        } else {
            panic!("Expected list result");
        }
    }
}
