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
<<<<<<< HEAD
            Ok(d) => d,
            Err(_e) => {
=======
            Ok(d) => if d.as_millis() > i64::MAX as u128 {
>>>>>>> feature/remove-unwrap-scalar-functions
                return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::OverflowError,
                });
            } else {
                d.as_millis() as i64
            },
            Err(_e) => {
                // This should never happen, since duration_since will ony return an error if the time is before the UNIX_EPOCH
                // return a zero duration this case
                0
            }
        };
        Ok(VariableValue::Integer(
            (since_epoch).into(),
        ))
    }
}
