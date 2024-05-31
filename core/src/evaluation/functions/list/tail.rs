use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Tail {}

#[async_trait]
impl ScalarFunction for Tail {
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
            VariableValue::List(l) => {
                if l.is_empty() {
                    return Ok(VariableValue::Null);
                }
                Ok(VariableValue::List(l[1..].to_vec()))
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(EvaluationError::InvalidType),
        }
    }
}
