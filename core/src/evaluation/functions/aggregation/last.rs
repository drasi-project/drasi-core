use std::{fmt::Debug, sync::Arc};

use crate::{
    evaluation::{FunctionError, FunctionEvaluationError},
    interface::ResultIndex,
    models::ElementValue,
};

use async_trait::async_trait;

use drasi_query_ast::ast;

use crate::evaluation::{variable_value::VariableValue, ExpressionEvaluationContext};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Last {}

#[async_trait]
impl AggregatingFunction for Last {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::Value(ElementValue::Null))
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
                function_name: "Last".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let value = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Value(value)) => value,
            _ => {
                return Err(FunctionError {
                    function_name: "Last".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                });
            }
        };
        

        *value = match (&args[0]).try_into() {
            Ok(value) => value,
            Err(_) => {
                return Err(FunctionError {
                    function_name: "Last".to_string(),
                    error: FunctionEvaluationError::InvalidType {
                        expected: "ElementValue".to_string(),
                    },
                })
            }
        };

        Ok((&value.clone()).into())
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Last".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let value = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Value(value)) => value,
            _ => {
            return Err(FunctionError {
                function_name: "Last".to_string(),
                error: FunctionEvaluationError::CorruptData,
            })
            }
        };

        *value = ElementValue::Null;
        Ok(VariableValue::Null)
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        let value = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Value(value)) => value,
            _ => {
                return Err(FunctionError {
                    function_name: "Last".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };
        Ok((&value.clone()).into())
    }
}

impl Debug for Last {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Last")
    }
}
