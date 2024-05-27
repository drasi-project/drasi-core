use std::{fmt::Debug, sync::Arc};

use crate::{evaluation::EvaluationError, interface::ResultIndex};

use async_trait::async_trait;

use drasi_query_ast::ast::Expression;

use crate::evaluation::{
    variable_value::float::Float, variable_value::VariableValue, ExpressionEvaluationContext,
};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Stdevp {}

#[async_trait]
impl AggregatingFunction for StdevP {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: &Vec<Expression>,
        _position_in_query: usize,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::Stdevp { sum: 0.0, count: 0 })
        // Accumulator::Value(ValueAccumulator::Stdevp { sum: 0.0, count: 0, sum_of_squares: 0.0 })
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
            return Err(EvaluationError::InvalidArgumentCount("stdevp".to_string()));
        }

        todo!();
    }
}