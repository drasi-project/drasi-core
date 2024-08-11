use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, FunctionEvaluationError,  ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Last {}

#[async_trait]
impl ScalarFunction for Last {
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
            VariableValue::List(l) => {
                if l.is_empty() {
                    return Ok(VariableValue::Null);
                }
                Ok(l[l.len() - 1].clone())
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
