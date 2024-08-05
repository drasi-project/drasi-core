use async_trait::async_trait;
use drasi_query_ast::ast::{self, Expression};
use rand::prelude::*;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Rand {}

#[async_trait]
impl ScalarFunction for Rand {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if !args.is_empty() {
            return Err(EvaluationError::InvalidArgumentCount("rand".to_string()));
        }
        let mut rng = rand::thread_rng();
        Ok(VariableValue::Float(match Float::from_f64(rng.gen()) {
            Some(f) => f,
            None => return Err(EvaluationError::FunctionError {
                function_name: expression.name.to_string(),
                error: Box::new(EvaluationError::ConversionError),
            }),
        }))
    }
}
