use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};
use async_trait::async_trait;
use drasi_query_ast::ast;

#[derive(Debug)]
pub struct Sign {}

#[async_trait]
impl ScalarFunction for Sign {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("sign".to_string()));
        }
        match &args[0] {
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::Integer(n) => {
                let f = n.as_i64().unwrap();
                if f > 0 {
                    Ok(VariableValue::Integer(Integer::from(1)))
                } else if f < 0 {
                    Ok(VariableValue::Integer(Integer::from(-1)))
                } else {
                    Ok(VariableValue::Integer(Integer::from(0)))
                }
            }
            VariableValue::Float(n) => {
                let f = n.as_f64().unwrap();
                if f > 0.0 {
                    Ok(VariableValue::Integer(Integer::from(1)))
                } else if f < 0.0 {
                    Ok(VariableValue::Integer(Integer::from(-1)))
                } else {
                    Ok(VariableValue::Integer(Integer::from(0)))
                }
            }
            _ => Err(EvaluationError::InvalidType),
        }
    }
}
