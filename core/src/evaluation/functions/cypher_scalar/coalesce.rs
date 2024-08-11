use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Coalesce {}

#[async_trait]
impl ScalarFunction for Coalesce {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        for arg in args {
            if arg != VariableValue::Null {
                return Ok(arg);
            }
        }

        Ok(VariableValue::Null)
    }
}
