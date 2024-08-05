use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Abs {}

#[async_trait]
impl ScalarFunction for Abs {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("abs".to_string()));
        }
        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(n) => {
                Ok(VariableValue::Integer(match n.as_i64() {
                    Some(i) => i.abs(),
                    None => return Err(EvaluationError::FunctionError { 
                        function_name: expression.name.to_string(),
                         error: Box::new(EvaluationError::ConversionError) }),
                }.into()))
            }
            VariableValue::Float(n) => Ok(VariableValue::Float(
                match Float::from_f64(match n.as_f64() {
                    Some(f) => f.abs(),
                    None => return Err(EvaluationError::FunctionError { 
                        function_name: expression.name.to_string(),
                         error: Box::new(EvaluationError::ConversionError) }),
                }) {
                    Some(f) => f,
                    None => return Err(EvaluationError::FunctionError { 
                        function_name: expression.name.to_string(),
                         error: Box::new(EvaluationError::ConversionError) }),
                }
            )),
            _ => Err(EvaluationError::InvalidType),
        }
    }
}
