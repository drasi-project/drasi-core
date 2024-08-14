use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Timestamp {}

#[async_trait]
impl ScalarFunction for Timestamp {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 0 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let now = std::time::SystemTime::now();
        let since_epoch = match now.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d,
            Err(_e) => {
                return Err(FunctionError {
                    function_name: "timestamp".to_string(),
                    error: FunctionEvaluationError::InvalidFormat { expected: "A valid Duration".to_string() },
                });
            }
        };
        Ok(VariableValue::Integer(
            (since_epoch.as_millis() as i64).into(),
        ))
    }
}
