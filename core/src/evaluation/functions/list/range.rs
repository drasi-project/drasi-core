use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

#[derive(Debug)]
pub struct Range {}

#[async_trait]
impl ScalarFunction for Range {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() < 2 || args.len() > 3 {
            return Err(EvaluationError::InvalidArgumentCount(
                expression.name.to_string(),
            ));
        }
        match args.len() {
            2 => match (&args[0], &args[1]) {
                (VariableValue::Integer(start), VariableValue::Integer(end)) => {
                    let mut range = Vec::new();
                    let start = start.as_i64().unwrap();
                    let end = end.as_i64().unwrap();
                    for i in start..end + 1 {
                        range.push(VariableValue::Integer(i.into()));
                    }
                    Ok(VariableValue::List(range))
                }
                _ => Err(EvaluationError::InvalidType),
            },
            3 => match (&args[0], &args[1], &args[2]) {
                (
                    VariableValue::Integer(start),
                    VariableValue::Integer(end),
                    VariableValue::Integer(step),
                ) => {
                    let mut range = Vec::new();
                    let start = start.as_i64().unwrap();
                    let end = end.as_i64().unwrap();
                    let step = step.as_i64().unwrap();
                    for i in (start..end + 1).step_by(step as usize) {
                        range.push(VariableValue::Integer(i.into()));
                    }
                    Ok(VariableValue::List(range))
                }
                _ => Err(EvaluationError::InvalidType),
            },
            _ => unreachable!(),
        }
    }
}
