use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{
    functions::ScalarFunction, variable_value::VariableValue, FunctionError,
    ExpressionEvaluationContext, FunctionEvaluationError,
};

pub struct Awaiting {}

impl Awaiting {
    pub fn new() -> Self {
        Awaiting {}
    }
}

#[async_trait]
impl ScalarFunction for Awaiting {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        Ok(VariableValue::Awaiting)
    }
}
