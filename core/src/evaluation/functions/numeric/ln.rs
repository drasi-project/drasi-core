use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

#[derive(Debug)]
pub struct Ln {}

#[async_trait]
impl ScalarFunction for Ln {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(n) => {
                let val = match n.as_i64() {
                    Some(i) => i as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                if val <= 0.0 {
                    return Ok(VariableValue::Null);
                }
                Ok(VariableValue::Float(
                    match Float::from_f64(val.ln()) {
                        Some(f) => f,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    },
                ))
            }
            VariableValue::Float(n) => {
                let val = match n.as_f64() {
                    Some(f) => f,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                if val <= 0.0 {
                    return Ok(VariableValue::Null);
                }
                Ok(VariableValue::Float(
                    match Float::from_f64(val.ln()) {
                        Some(f) => f,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    },
                ))
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
