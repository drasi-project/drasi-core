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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::context::QueryVariables;
    use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};
    use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn test_collect_basic() {
        let collect = Collect {};
        let variables = QueryVariables::new();
        let context = ExpressionEvaluationContext::new(
            &variables,
            Arc::new(InstantQueryClock::new(0, 0)),
        );
        let expression = ast::FunctionExpression {
            name: Arc::from("collect"),
            args: vec![],
            position_in_query: 0,
        };
        
        let index = Arc::new(InMemoryResultIndex::new());
        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply first value
        let result = collect
            .apply(
                &context,
                vec![VariableValue::String("test1".to_string())],
                &mut accumulator,
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            VariableValue::List(vec![VariableValue::String("test1".to_string())])
        );

        // Apply second value
        let result = collect
            .apply(
                &context,
                vec![VariableValue::String("test2".to_string())],
                &mut accumulator,
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            VariableValue::List(vec![
                VariableValue::String("test1".to_string()),
                VariableValue::String("test2".to_string())
            ])
        );

        // Snapshot
        let snapshot = collect
            .snapshot(&context, vec![VariableValue::String("test2".to_string())], &accumulator)
            .await
            .unwrap();

        assert_eq!(
            snapshot,
            VariableValue::List(vec![
                VariableValue::String("test1".to_string()),
                VariableValue::String("test2".to_string())
            ])
        );

        // Revert first value
        let result = collect
            .revert(
                &context,
                vec![VariableValue::String("test1".to_string())],
                &mut accumulator,
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            VariableValue::List(vec![VariableValue::String("test2".to_string())])
        );
    }

    #[tokio::test]
    async fn test_collect_with_objects() {
        let collect = Collect {};
        let variables = QueryVariables::new();
        let context = ExpressionEvaluationContext::new(
            &variables,
            Arc::new(InstantQueryClock::new(0, 0)),
        );
        let expression = ast::FunctionExpression {
            name: Arc::from("collect"),
            args: vec![],
            position_in_query: 0,
        };
        
        let index = Arc::new(InMemoryResultIndex::new());
        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Create an object
        let mut obj1 = BTreeMap::new();
        obj1.insert("id".to_string(), VariableValue::String("1".to_string()));
        obj1.insert("value".to_string(), VariableValue::Integer(42.into()));

        let result = collect
            .apply(&context, vec![VariableValue::Object(obj1.clone())], &mut accumulator)
            .await
            .unwrap();

        assert_eq!(result, VariableValue::List(vec![VariableValue::Object(obj1)]));
    }

    #[tokio::test]
    async fn test_collect_null_values() {
        let collect = Collect {};
        let variables = QueryVariables::new();
        let context = ExpressionEvaluationContext::new(
            &variables,
            Arc::new(InstantQueryClock::new(0, 0)),
        );
        let expression = ast::FunctionExpression {
            name: Arc::from("collect"),
            args: vec![],
            position_in_query: 0,
        };
        
        let index = Arc::new(InMemoryResultIndex::new());
        let mut accumulator = collect.initialize_accumulator(&context, &expression, &vec![], index);

        // Apply null value - should not add to list
        let result = collect
            .apply(&context, vec![VariableValue::Null], &mut accumulator)
            .await
            .unwrap();

        assert_eq!(result, VariableValue::List(vec![]));
    }
}
