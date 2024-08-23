use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};
use async_trait::async_trait;
use drasi_query_ast::ast;

#[derive(Debug)]
pub struct Sign {}

#[async_trait]
impl ScalarFunction for Sign {
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
                let f = match n.as_i64() {
                    Some(i) => i,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                if f > 0 {
                    Ok(VariableValue::Integer(Integer::from(1)))
                } else if f < 0 {
                    Ok(VariableValue::Integer(Integer::from(-1)))
                } else {
                    Ok(VariableValue::Integer(Integer::from(0)))
                }
            }
            VariableValue::Float(n) => {
                let f = match n.as_f64() {
                    Some(f) => f,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                if f > 0.0 {
                    Ok(VariableValue::Integer(Integer::from(1)))
                } else if f < 0.0 {
                    Ok(VariableValue::Integer(Integer::from(-1)))
                } else {
                    Ok(VariableValue::Integer(Integer::from(0)))
                }
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
