use std::{fmt::Debug, sync::Arc};

use crate::{
    evaluation::{FunctionError, FunctionEvaluationError},
    interface::ResultIndex,
};

use async_trait::async_trait;

use drasi_query_ast::ast;

use crate::evaluation::{
    variable_value::integer::Integer, variable_value::VariableValue, ExpressionEvaluationContext,
};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Count {}

#[async_trait]
impl AggregatingFunction for Count {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::Count { value: 0 })
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
                function_name: "Count".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let value = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Count { value } ) => value,
            _ => {
                return Err(FunctionError {
                    function_name: "Count".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Integer(Integer::from(*value))),
            _ => {
                *value += 1;
                Ok(VariableValue::Integer(Integer::from(*value)))
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
                function_name: "Count".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let value = if let Accumulator::Value(super::ValueAccumulator::Count { value }) = accumulator {
            value
        } else {
            return Err(FunctionError {
            function_name: "Count".to_string(),
            error: FunctionEvaluationError::CorruptData,
            });
        };

        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Integer(Integer::from(*value))),
            _ => {
                *value -= 1;
                Ok(VariableValue::Integer(Integer::from(*value)))
            }
        }
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        let value = if let Accumulator::Value(super::ValueAccumulator::Count { value }) = accumulator {
            value
        } else {
            return Err(FunctionError {
            function_name: "Count".to_string(),
            error: FunctionEvaluationError::CorruptData,
            });
        };
        Ok(VariableValue::Integer(Integer::from(*value)))
    }
}

impl Debug for Count {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Count")
    }
}
