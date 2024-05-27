use std::{fmt::Debug, sync::Arc};

use crate::{evaluation::EvaluationError, interface::ResultIndex};

use async_trait::async_trait;

use drasi_query_ast::ast::Expression;

use crate::evaluation::{
    variable_value::duration::Duration, variable_value::float::Float,
    variable_value::VariableValue, ExpressionEvaluationContext,
};

use chrono::Duration as ChronoDuration;

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Avg {}

#[async_trait]
impl AggregatingFunction for Avg {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _args: &Vec<Expression>,
        _position_in_query: usize,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::Avg { sum: 0.0, count: 0 })
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
            return Err(EvaluationError::InvalidArgumentCount("avg".to_string()));
        }

        let (sum, count) = match accumulator {
            Accumulator::Value(accumulator) => match accumulator {
                ValueAccumulator::Avg { sum, count } => (sum, count),
                _ => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *count += 1;
                *sum += n.as_f64().unwrap();
                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Integer(n) => {
                *count += 1;
                *sum += n.as_i64().unwrap() as f64;
                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            // The average of two dates/times does not really make sense
            // Only adding duration for now
            VariableValue::Duration(d) => {
                *count += 1;
                *sum += d.duration().num_milliseconds() as f64;
                let avg = *sum / *count as f64;

                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::milliseconds(avg as i64),
                    0,
                    0,
                )))
            }
            _ => Err(EvaluationError::InvalidType),
        }
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Avg".to_string()));
        }
        let (sum, count) = match accumulator {
            Accumulator::Value(accumulator) => match accumulator {
                ValueAccumulator::Avg { sum, count } => (sum, count),
                _ => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *count -= 1;
                *sum -= n.as_f64().unwrap();

                if *count == 0 {
                    return Ok(VariableValue::Float(
                        Float::from_f64(0.0).unwrap_or_default(),
                    ));
                }

                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Integer(n) => {
                *count -= 1;
                *sum -= n.as_i64().unwrap() as f64;

                if *count == 0 {
                    return Ok(VariableValue::Float(
                        Float::from_f64(0.0).unwrap_or_default(),
                    ));
                }

                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Duration(d) => {
                *count -= 1;
                *sum -= d.duration().num_milliseconds() as f64;

                if *count == 0 {
                    return Ok(VariableValue::Float(
                        Float::from_f64(0.0).unwrap_or_default(),
                    ));
                }

                let avg = *sum / *count as f64;

                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::milliseconds(avg as i64),
                    0,
                    0,
                )))
            }
            _ => Err(EvaluationError::InvalidType),
        }
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Avg".to_string()));
        }
        let (sum, count) = match accumulator {
            Accumulator::Value(accumulator) => match accumulator {
                ValueAccumulator::Avg { sum, count } => (sum, count),
                _ => return Err(EvaluationError::InvalidType),
            },
            _ => return Err(EvaluationError::InvalidType),
        };

        if *count == 0 {
            return Ok(VariableValue::Float(
                Float::from_f64(0.0).unwrap_or_default(),
            ));
        }

        let avg = *sum / *count as f64;

        match &args[0] {
            VariableValue::Float(_) => Ok(VariableValue::Float(Float::from_f64(avg).unwrap())),
            VariableValue::Integer(_) => Ok(VariableValue::Float(Float::from_f64(avg).unwrap())),
            VariableValue::Duration(_) => Ok(VariableValue::Duration(Duration::new(
                ChronoDuration::milliseconds(avg as i64),
                0,
                0,
            ))),
            _ => Err(EvaluationError::InvalidType),
        }
    }
}

impl Debug for Avg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Avg")
    }
}
