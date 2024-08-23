use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

#[derive(Debug)]
pub struct ToFloat {}

#[async_trait]
impl ScalarFunction for ToFloat {
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
            VariableValue::Integer(i) => Ok(VariableValue::Float(
                (match i.as_i64() {
                    Some(i) => i,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                } as f64)
                    .into(),
            )),
            VariableValue::Float(f) => Ok(VariableValue::Float(f.clone())),
            VariableValue::String(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Ok(VariableValue::Float((i as f64).into()))
                } else if let Ok(f) = s.parse::<f64>() {
                    Ok(VariableValue::Float(f.into()))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct ToFloatOrNull {}

#[async_trait]
impl ScalarFunction for ToFloatOrNull {
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
            VariableValue::Integer(i) => Ok(VariableValue::Float(
                (match i.as_i64() {
                    Some(i) => i,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                } as f64)
                    .into(),
            )),
            VariableValue::Float(f) => Ok(VariableValue::Float(f.clone())),
            VariableValue::String(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Ok(VariableValue::Float((i as f64).into()))
                } else if let Ok(f) = s.parse::<f64>() {
                    Ok(VariableValue::Float(f.into()))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            _ => Ok(VariableValue::Null),
        }
    }
}
