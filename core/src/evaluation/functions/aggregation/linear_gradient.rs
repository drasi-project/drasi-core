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

use crate::evaluation::{
    variable_value::float::Float, variable_value::VariableValue, ExpressionEvaluationContext,
};

use chrono::{NaiveTime, Timelike};

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct LinearGradient {}

#[async_trait]
impl AggregatingFunction for LinearGradient {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::LinearGradient {
            count: 0,
            mean_x: 0.0,
            mean_y: 0.0,
            m2: 0.0,
            cov: 0.0,
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
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: "linearGradient".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let (count, mean_x, mean_y, m2, cov) = match accumulator {
            Accumulator::Value(ValueAccumulator::LinearGradient {
                count,
                mean_x,
                mean_y,
                m2,
                cov,
            }) => (count, mean_x, mean_y, m2, cov),
            _ => {
                return Err(FunctionError {
                    function_name: "LinearGradient".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        if let VariableValue::Null = args[0] {
            return Ok(VariableValue::Null);
        }

        if let VariableValue::Null = args[1] {
            return Ok(VariableValue::Null);
        }

        let x = extract_parameter(&args[0], 0)?;
        let y = extract_parameter(&args[1], 1)?;

        *count += 1;
        let delta_x = x - *mean_x;
        let delta_y = y - *mean_y;
        *mean_x += delta_x / *count as f64;
        *mean_y += delta_y / *count as f64;
        let delta2 = x - *mean_x;
        *m2 += delta_x * delta2;
        *cov += delta_x * (y - *mean_y);

        let result = covariance(*cov, *count) / variance(*m2, *count);

        if result.is_nan() {
            return Ok(VariableValue::Null);
        }

        Ok(VariableValue::Float(
            Float::from_f64(result).unwrap_or_default(),
        ))
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: "linearGradient".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let (count, mean_x, mean_y, m2, cov) = match accumulator {
            Accumulator::Value(ValueAccumulator::LinearGradient {
                count,
                mean_x,
                mean_y,
                m2,
                cov,
            }) => (count, mean_x, mean_y, m2, cov),
            _ => {
                return Err(FunctionError {
                    function_name: "LinearGradient".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                });
            }
        };

        if let VariableValue::Null = args[0] {
            return Ok(VariableValue::Null);
        }

        if let VariableValue::Null = args[1] {
            return Ok(VariableValue::Null);
        }

        let x = extract_parameter(&args[0], 0)?;
        let y = extract_parameter(&args[1], 1)?;

        *count -= 1;

        if *count == 0 {
            *mean_x = 0.0;
            *mean_y = 0.0;
            *m2 = 0.0;
            *cov = 0.0;
            return Ok(VariableValue::Null);
        }

        let delta_x = x - *mean_x;
        let delta_y = y - *mean_y;
        *mean_x -= delta_x / *count as f64;
        *mean_y -= delta_y / *count as f64;
        let delta2 = x - *mean_x;
        *m2 -= delta_x * delta2;
        *cov -= delta_x * (y - *mean_y);

        let result = covariance(*cov, *count) / variance(*m2, *count);

        if result.is_nan() {
            return Ok(VariableValue::Null);
        }

        Ok(VariableValue::Float(
            Float::from_f64(result).unwrap_or_default(),
        ))
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: "linearGradient".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let (count, _mean_x, _mean_y, m2, cov) = match accumulator {
            Accumulator::Value(ValueAccumulator::LinearGradient {
                count,
                mean_x,
                mean_y,
                m2,
                cov,
            }) => (count, mean_x, mean_y, m2, cov),
            _ => {
                return Err(FunctionError {
                    function_name: "LinearGradient".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                });
            }
        };

        if *count == 0 {
            return Ok(VariableValue::Null);
        }

        let result = covariance(*cov, *count) / variance(*m2, *count);

        if result.is_nan() {
            return Ok(VariableValue::Null);
        }

        Ok(VariableValue::Float(
            Float::from_f64(result).unwrap_or_default(),
        ))
    }
}

fn extract_parameter(p: &VariableValue, index: u64) -> Result<f64, FunctionError> {
    let result = match p {
        VariableValue::Float(n) => match n.as_f64() {
            Some(n) => n,
            None => {
                return Err(FunctionError {
                    function_name: "LinearGradient".to_string(),
                    error: FunctionEvaluationError::OverflowError,
                })
            }
        },
        VariableValue::Integer(n) => match n.as_i64() {
            Some(n) => n as f64,
            None => {
                return Err(FunctionError {
                    function_name: "LinearGradient".to_string(),
                    error: FunctionEvaluationError::OverflowError,
                })
            }
        },
        VariableValue::Duration(d) => d.duration().num_milliseconds() as f64,
        VariableValue::LocalDateTime(l) => l.and_utc().timestamp_millis() as f64,
        VariableValue::ZonedDateTime(z) => z.datetime().timestamp_millis() as f64,
        VariableValue::Date(d) => d.and_time(NaiveTime::MIN).and_utc().timestamp_millis() as f64,
        VariableValue::LocalTime(l) => l.num_seconds_from_midnight() as f64,
        VariableValue::ZonedTime(z) => z.time().num_seconds_from_midnight() as f64,
        _ => {
            return Err(FunctionError {
                function_name: "LinearGradient".to_string(),
                error: FunctionEvaluationError::InvalidArgument(index as usize),
            })
        }
    };

    Ok(result)
}

fn variance(m2: f64, count: i64) -> f64 {
    if count < 2 {
        return 0.0;
    }
    m2 / (count - 1) as f64
}

fn covariance(cov: f64, count: i64) -> f64 {
    if count < 2 {
        return 0.0;
    }
    cov / (count - 1) as f64
}

impl Debug for LinearGradient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LinearGradient")
    }
}
