use std::{fmt::Debug, sync::Arc};

use crate::{evaluation::EvaluationError, interface::ResultIndex};

use async_trait::async_trait;

use drasi_query_ast::ast::Expression;

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
        _args: &Vec<Expression>,
        _position_in_query: usize,
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
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Count".to_string()));
        }

        let value = match accumulator {
            Accumulator::Value(accumulator) => match accumulator {
                super::ValueAccumulator::Count { value } => value,
                _ => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
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
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Count".to_string()));
        }
        let value = match accumulator {
            Accumulator::Value(accumulator) => match accumulator {
                super::ValueAccumulator::Count { value } => value,
                _ => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        // if *value == 0 {
        //     println!("Count is already 0");
        //     return Ok(VariableValue::Integer(Integer::from(*value)));
        // }

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
    ) -> Result<VariableValue, EvaluationError> {
        let value = match accumulator {
            Accumulator::Value(accumulator) => match accumulator {
                super::ValueAccumulator::Count { value } => value,
                _ => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };
        Ok(VariableValue::Integer(Integer::from(*value)))
    }
}

impl Debug for Count {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Count")
    }
}
