use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Timestamp {}

#[async_trait]
impl ScalarFunction for Timestamp {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 0 {
            return Err(EvaluationError::InvalidArgumentCount("timestamp".to_string()));
        }
        let now = std::time::SystemTime::now();
        let since_epoch = now.duration_since(std::time::UNIX_EPOCH).unwrap();
        Ok(VariableValue::Integer((since_epoch.as_millis() as i64).into()))
    }
}