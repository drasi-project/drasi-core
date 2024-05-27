use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct ToBoolean {}

#[async_trait]
impl ScalarFunction for ToBoolean {
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
            VariableValue::Integer(i) => Ok(VariableValue::Bool(i.as_i64().unwrap() != 0)),
            VariableValue::String(s) => {
                if (s == "true") || (s == "false") {
                    Ok(VariableValue::Bool(s == "true"))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            VariableValue::Bool(b) => Ok(VariableValue::Bool(*b)),
            _ => Err(EvaluationError::FunctionError {
                function_name: expression.name.to_string(),
                error: Box::new(EvaluationError::InvalidType),
            }),
        }
    }
}

#[derive(Debug)]
pub struct ToBooleanOrNull {}

#[async_trait]
impl ScalarFunction for ToBooleanOrNull {
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
            VariableValue::Integer(i) => Ok(VariableValue::Bool(i.as_i64().unwrap() != 0)),
            VariableValue::String(s) => {
                if (s == "true") || (s == "false") {
                    Ok(VariableValue::Bool(s == "true"))
                } else {
                    Ok(VariableValue::Null)
                }
            }
            VariableValue::Bool(b) => Ok(VariableValue::Bool(*b)),
            _ => Ok(VariableValue::Null),
        }
    }
}
