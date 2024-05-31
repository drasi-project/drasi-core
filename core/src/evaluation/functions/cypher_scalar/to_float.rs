use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct ToFloat {}

#[async_trait]
impl ScalarFunction for ToFloat {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                expression.name.to_string(),
            ));
        }
        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(i) => {
                Ok(VariableValue::Float((i.as_i64().unwrap() as f64).into()))
            }
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
            _ => Err(EvaluationError::FunctionError {
                function_name: expression.name.to_string(),
                error: Box::new(EvaluationError::InvalidType),
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
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                expression.name.to_string(),
            ));
        }
        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(i) => {
                Ok(VariableValue::Float((i.as_i64().unwrap() as f64).into()))
            }
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
