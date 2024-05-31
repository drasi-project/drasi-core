use async_trait::async_trait;
use drasi_query_ast::ast;
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 0 {
            return Err(EvaluationError::InvalidArgumentCount("rand".to_string()));
        }
        let mut rng = rand::thread_rng();
        Ok(VariableValue::Float(Float::from_f64(rng.gen()).unwrap()))
    }
}
