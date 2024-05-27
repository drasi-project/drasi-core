use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Floor {}

#[async_trait]
impl ScalarFunction for Floor {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("floor".to_string()));
        }
        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(n) => {
                Ok(VariableValue::Integer(Integer::from(n.as_i64().unwrap())))
            }
            VariableValue::Float(n) => Ok(VariableValue::Float(
                Float::from_f64(n.as_f64().unwrap().floor()).unwrap(),
            )),
            _ => Err(EvaluationError::InvalidType),
        }
    }
}