use async_trait::async_trait;
use drasi_query_ast::ast;
use rand::prelude::*;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Rand {}

#[async_trait]
impl ScalarFunction for Rand {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if !args.is_empty() {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let mut rng = rand::thread_rng();
        Ok(VariableValue::Float(Float::from_f64(rng.gen()).unwrap()))
    }
}
