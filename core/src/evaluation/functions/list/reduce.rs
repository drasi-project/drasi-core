use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::fmt::Formatter;
use std::sync::Arc;

use crate::evaluation::functions::LazyScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator};
use drasi_query_ast::ast::{self, Expression};

pub struct Reduce {
    evaluator: Lazy<ExpressionEvaluator>,
}

impl Reduce {
    pub fn new() -> Self {
        Reduce {
            evaluator: Lazy::new(|| {
                let function_registry = Arc::new(FunctionRegistry::new());
                let ari = Arc::new(InMemoryResultIndex::new());
                ExpressionEvaluator::new(function_registry, ari)
            }),
        }
    }
}

#[async_trait]
impl LazyScalarFunction for Reduce {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: &Vec<ast::Expression>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 2 {
            return Err(EvaluationError::InvalidArgumentCount("reduce".to_string()));
        }

        let initializer = &args[0];
        let iterator = match &args[1] {
            Expression::IteratorExpression(i) => i,
            _ => return Err(EvaluationError::InvalidType),
        };

        let (accumulator_name, accumulator) = self
            .evaluator
            .evaluate_assignment(context, initializer)
            .await?;

        let mut query_variables = context.clone_variables(); //Retrieve the query variables from the global context
        query_variables.insert(accumulator_name.to_string().into(), accumulator);

        let context = ExpressionEvaluationContext::new(&query_variables, context.get_clock());
        let result = self
            .evaluator
            .reduce_iterator_expression(&context, iterator, accumulator_name)
            .await?;

        Ok(result)
    }
}

impl std::fmt::Debug for Reduce {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reduce")
    }
}
