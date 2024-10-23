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
        _expression: &ast::FunctionExpression,
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
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let (sum, count) = match accumulator {
            Accumulator::Value(ValueAccumulator::Avg { sum, count }) => (sum, count),
            _ => {
                return Err(FunctionError {
                    function_name: "Avg".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *count += 1;
                *sum += match n.as_f64() {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Integer(n) => {
                *count += 1;
                *sum += match n.as_i64() {
                    Some(n) => n as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
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
            VariableValue::Null => {
                let avg = *sum / *count as f64;
                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            _ => Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
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
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let (sum, count) = match accumulator {
            Accumulator::Value(ValueAccumulator::Avg { sum, count }) => (sum, count),
            _ => {
                return Err(FunctionError {
                    function_name: "Avg".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *count -= 1;
                *sum -= match n.as_f64() {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };

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
                *sum -= match n.as_i64() {
                    Some(n) => n as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };

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
            VariableValue::Null => {
                let avg = *sum / *count as f64;
                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            _ => Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let (sum, count) = match accumulator {
            Accumulator::Value(ValueAccumulator::Avg { sum, count }) => (sum, count),
            _ => {
                return Err(FunctionError {
                    function_name: "Avg".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        if *count == 0 {
            return Ok(VariableValue::Float(
                Float::from_f64(0.0).unwrap_or_default(),
            ));
        }

        let avg = *sum / *count as f64;

        match &args[0] {
            VariableValue::Float(_) => Ok(VariableValue::Float(
                Float::from_f64(avg).unwrap_or_default(),
            )),
            VariableValue::Integer(_) => Ok(VariableValue::Float(
                Float::from_f64(avg).unwrap_or_default(),
            )),
            VariableValue::Duration(_) => Ok(VariableValue::Duration(Duration::new(
                ChronoDuration::milliseconds(avg as i64),
                0,
                0,
            ))),
            _ => Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

impl Debug for Avg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Avg")
    }
}
