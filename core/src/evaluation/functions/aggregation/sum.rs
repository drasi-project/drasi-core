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

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};
use chrono::Duration as ChronoDuration;

#[derive(Clone)]
pub struct Sum {}

#[async_trait]
impl AggregatingFunction for Sum {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::Sum { value: 0.0 })
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
                function_name: "Sum".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let accumulator = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Sum { value }) => value,
            _ => {
                return Err(FunctionError {
                    function_name: "Sum".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                });
            }
        };
        

        match &args[0] {
            VariableValue::Float(n) => {
                *accumulator += match n.as_f64() {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                Ok(VariableValue::Float(match Float::from_f64(*accumulator) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                }))
            }
            VariableValue::Integer(n) => {
                *accumulator += match n.as_i64() {
                    Some(n) => n as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                Ok(VariableValue::Float(match Float::from_f64(*accumulator) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                }))
            }
            VariableValue::Duration(d) => {
                *accumulator += d.duration().num_milliseconds() as f64;
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::milliseconds(*accumulator as i64),
                    0,
                    0,
                )))
            }
            VariableValue::Null => Ok(VariableValue::Float(match Float::from_f64(*accumulator) {
                Some(n) => n,
                None => {
                    return Err(FunctionError {
                        function_name: "Sum".to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    })
                }
            })),
            _ => Err(FunctionError {
                function_name: "Sum".to_string(),
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
                function_name: "Sum".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let accumulator = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Sum { value }) => value,
            _ => {
            return Err(FunctionError {
                function_name: "Sum".to_string(),
                error: FunctionEvaluationError::CorruptData,
            })
            }
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *accumulator -= match n.as_f64() {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                Ok(VariableValue::Float(match Float::from_f64(*accumulator) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                }))
            }
            VariableValue::Integer(n) => {
                *accumulator -= match n.as_i64() {
                    Some(n) => n as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                Ok(VariableValue::Float(match Float::from_f64(*accumulator) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                }))
            }
            VariableValue::Duration(d) => {
                *accumulator -= d.duration().num_milliseconds() as f64;
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::milliseconds(*accumulator as i64),
                    0,
                    0,
                )))
            }
            VariableValue::Null => Ok(VariableValue::Float(match Float::from_f64(*accumulator) {
                Some(n) => n,
                None => {
                    return Err(FunctionError {
                        function_name: "Sum".to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    })
                }
            })),
            _ => Err(FunctionError {
                function_name: "Sum".to_string(),
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
                function_name: "Sum".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let accumulator_value = match accumulator {
            Accumulator::Value(super::ValueAccumulator::Sum { value }) => value,
            _ => {
                return Err(FunctionError {
                    function_name: "Sum".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                });
            }
        };
        

        match &args[0] {
            VariableValue::Float(_) => Ok(VariableValue::Float(
                match Float::from_f64(*accumulator_value) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                },
            )),
            VariableValue::Integer(_) => Ok(VariableValue::Float(
                match Float::from_f64(*accumulator_value) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                },
            )),
            VariableValue::Duration(_) => Ok(VariableValue::Duration(Duration::new(
                ChronoDuration::milliseconds(*accumulator_value as i64),
                0,
                0,
            ))),
            VariableValue::Null => Ok(VariableValue::Float(
                match Float::from_f64(*accumulator_value) {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Sum".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                },
            )),
            _ => Err(FunctionError {
                function_name: "Sum".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

impl Debug for Sum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sum")
    }
}
