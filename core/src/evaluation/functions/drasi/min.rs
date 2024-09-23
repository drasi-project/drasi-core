use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, FunctionEvaluationError};
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{variable_value::VariableValue, ExpressionEvaluationContext};

#[derive(Clone)]
pub struct DrasiMin {}

#[async_trait]
impl ScalarFunction for DrasiMin {
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
                let mut result = l[0].clone();
                for element in l {
                    if element == &VariableValue::Null {
                        continue;
                    }
                    if element < &result {
                        result = element.clone();
                    }
                }
                Ok(result.clone())
            }
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::String(s) => Ok(VariableValue::String(s.clone())),
            VariableValue::Float(f) => Ok(VariableValue::Float(f.clone())),
            VariableValue::Integer(i) => Ok(VariableValue::Integer(i.clone())),
            VariableValue::Bool(b) => Ok(VariableValue::Bool(*b)),
            VariableValue::Object(o) => Ok(VariableValue::Object(o.clone())),
            VariableValue::Date(d) => Ok(VariableValue::Date(*d)),
            VariableValue::LocalTime(t) => Ok(VariableValue::LocalTime(*t)),
            VariableValue::LocalDateTime(dt) => Ok(VariableValue::LocalDateTime(*dt)),
            VariableValue::ZonedTime(t) => Ok(VariableValue::ZonedTime(*t)),
            VariableValue::ZonedDateTime(dt) => Ok(VariableValue::ZonedDateTime(dt.clone())),
            VariableValue::Duration(d) => Ok(VariableValue::Duration(d.clone())),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
