use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct ToInteger {}

#[async_trait]
impl ScalarFunction for ToInteger {
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
            VariableValue::Integer(i) => Ok(VariableValue::Integer(i.clone())),
            VariableValue::Float(f) => Ok(VariableValue::Integer(
                (f.as_f64().unwrap().floor() as i64).into(),
            )),
            VariableValue::Bool(b) => {
                if *b {
                    Ok(VariableValue::Integer(1.into()))
                } else {
                    Ok(VariableValue::Integer(0.into()))
                }
            }
            VariableValue::String(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Ok(VariableValue::Integer(i.into()))
                } else if let Ok(f) = s.parse::<f64>() {
                    Ok(VariableValue::Integer((f.floor() as i64).into()))
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
pub struct ToIntegerOrNull {}

#[async_trait]
impl ScalarFunction for ToIntegerOrNull {
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
            VariableValue::Integer(i) => Ok(VariableValue::Integer(i.clone())),
            VariableValue::Float(f) => Ok(VariableValue::Integer(
                (f.as_f64().unwrap().floor() as i64).into(),
            )),
            VariableValue::Bool(b) => {
                if *b {
                    Ok(VariableValue::Integer(1.into()))
                } else {
                    Ok(VariableValue::Integer(0.into()))
                }
            }
            VariableValue::String(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Ok(VariableValue::Integer(i.into()))
                } else if let Ok(f) = s.parse::<f64>() {
                    Ok(VariableValue::Integer((f.floor() as i64).into()))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            _ => Ok(VariableValue::Null),
        }
    }
}
