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

use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::{
    evaluation::{
        variable_value::VariableValue, ExpressionEvaluationContext, FunctionError,
        FunctionEvaluationError,
    },
    interface::ResultIndex,
    models::ElementValue,
};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

/// Collect aggregation function that collects all values into a list
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
                function_name: "Collect".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let list = match accumulator {
            Accumulator::Value(ValueAccumulator::Value(ElementValue::List(list))) => list,
            _ => {
                return Err(FunctionError {
                    function_name: "Collect".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        // Convert VariableValue to ElementValue and add to list
        // Skip null values (similar to how other aggregation functions handle nulls)
        if !args[0].is_null() {
            if let Ok(elem_value) = (&args[0]).try_into() {
                list.push(elem_value);
            }
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
                function_name: "Collect".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let list = match accumulator {
            Accumulator::Value(ValueAccumulator::Value(ElementValue::List(list))) => list,
            _ => {
                return Err(FunctionError {
                    function_name: "Collect".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        // For revert, we need to remove the value from the list
        // This is tricky because we need to find and remove the exact value
        // For now, we'll remove the first occurrence
        if !args[0].is_null() {
            if let Ok(elem_value) = (&args[0]).try_into() {
                // Find and remove the first matching value
                if let Some(pos) = list.iter().position(|x| x == &elem_value) {
                    list.remove(pos);
                }
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
        let list = match accumulator {
            Accumulator::Value(ValueAccumulator::Value(ElementValue::List(list))) => list,
            _ => {
                return Err(FunctionError {
                    function_name: "Collect".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        Ok((&ElementValue::List(list.clone())).into())
    }
}

impl Debug for Collect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Collect")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        evaluation::{
            context::QueryVariables, variable_value::VariableValue, ExpressionEvaluationContext,
            InstantQueryClock,
        },
        in_memory_index::in_memory_result_index::InMemoryResultIndex,
    };
    use drasi_query_ast::ast;

    #[tokio::test]
    async fn test_collect_basic() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        // Initialize accumulator
        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply some values
        let val1 = VariableValue::String("hello".into());
        let val2 = VariableValue::Integer(42.into());
        let val3 = VariableValue::String("world".into());

        let _ = collect
            .apply(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val2.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val3.clone()], &mut accumulator)
            .await
            .unwrap();

        // Snapshot should return all values
        let result = collect
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

    #[tokio::test]
    async fn test_collect_with_revert() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        // Initialize accumulator
        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply some values
        let val1 = VariableValue::String("hello".into());
        let val2 = VariableValue::Integer(42.into());

        let _ = collect
            .apply(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val2.clone()], &mut accumulator)
            .await
            .unwrap();

        // Revert one value
        let _ = collect
            .revert(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();

        // Snapshot should return only remaining value
        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0], val2);
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_null_values() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply null values - they should be ignored
        let _ = collect
            .apply(&context, vec![VariableValue::Null], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(
                &context,
                vec![VariableValue::Integer(42.into())],
                &mut accumulator,
            )
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![VariableValue::Null], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(
                &context,
                vec![VariableValue::String("test".into())],
                &mut accumulator,
            )
            .await
            .unwrap();

        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 2, "Null values should be ignored");
            assert_eq!(list[0], VariableValue::Integer(42.into()));
            assert_eq!(list[1], VariableValue::String("test".into()));
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_empty_list() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Snapshot of empty accumulator should return empty list
        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 0, "Empty accumulator should return empty list");
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_duplicate_values() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply duplicate values - they should all be collected
        let val = VariableValue::Integer(42.into());
        let _ = collect
            .apply(&context, vec![val.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val.clone()], &mut accumulator)
            .await
            .unwrap();

        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 3, "Duplicate values should all be collected");
            assert_eq!(list[0], val);
            assert_eq!(list[1], val);
            assert_eq!(list[2], val);
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_different_types() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply values of different types
        let _ = collect
            .apply(
                &context,
                vec![VariableValue::Integer(42.into())],
                &mut accumulator,
            )
            .await
            .unwrap();
        let _ = collect
            .apply(
                &context,
                vec![VariableValue::Float(3.125.into())],
                &mut accumulator,
            )
            .await
            .unwrap();
        let _ = collect
            .apply(
                &context,
                vec![VariableValue::String("hello".into())],
                &mut accumulator,
            )
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![VariableValue::Bool(true)], &mut accumulator)
            .await
            .unwrap();

        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 4, "Should collect values of different types");
            assert_eq!(list[0], VariableValue::Integer(42.into()));
            assert_eq!(list[1], VariableValue::Float(3.125.into()));
            assert_eq!(list[2], VariableValue::String("hello".into()));
            assert_eq!(list[3], VariableValue::Bool(true));
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_revert_multiple() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply values including duplicates
        let val1 = VariableValue::Integer(1.into());
        let val2 = VariableValue::Integer(2.into());

        let _ = collect
            .apply(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val2.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();
        let _ = collect
            .apply(&context, vec![val2.clone()], &mut accumulator)
            .await
            .unwrap();

        // Revert one instance of val1
        let _ = collect
            .revert(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();

        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(list.len(), 3, "Should have removed only first occurrence");
            assert_eq!(list[0], val2); // First val1 was removed
            assert_eq!(list[1], val1); // Second val1 remains
            assert_eq!(list[2], val2);
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_revert_nonexistent() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        let val1 = VariableValue::Integer(1.into());
        let val2 = VariableValue::Integer(2.into());

        let _ = collect
            .apply(&context, vec![val1.clone()], &mut accumulator)
            .await
            .unwrap();

        // Try to revert a value that doesn't exist
        let _ = collect
            .revert(&context, vec![val2.clone()], &mut accumulator)
            .await
            .unwrap();

        let result = collect
            .snapshot(&context, vec![], &accumulator)
            .await
            .unwrap();

        if let VariableValue::List(list) = result {
            assert_eq!(
                list.len(),
                1,
                "Should not affect list if value doesn't exist"
            );
            assert_eq!(list[0], val1);
        } else {
            panic!("Expected list result");
        }
    }

    #[tokio::test]
    async fn test_collect_error_cases() {
        let collect = Collect {};
        let index = Arc::new(InMemoryResultIndex::new());
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let expression = ast::FunctionExpression {
            name: "collect".into(),
            args: vec![],
            position_in_query: 10,
        };

        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Test with wrong number of arguments
        let result = collect.apply(&context, vec![], &mut accumulator).await;
        assert!(result.is_err(), "Should error with no arguments");

        let result = collect
            .apply(
                &context,
                vec![
                    VariableValue::Integer(1.into()),
                    VariableValue::Integer(2.into()),
                ],
                &mut accumulator,
            )
            .await;
        assert!(result.is_err(), "Should error with too many arguments");
    }
}
