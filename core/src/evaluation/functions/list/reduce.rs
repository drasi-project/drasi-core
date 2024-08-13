use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::fmt::Formatter;
use std::sync::Arc;

use crate::evaluation::functions::LazyScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext, ExpressionEvaluator};
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
        expression: &ast::FunctionExpression,
        args: &Vec<ast::Expression>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let initializer = &args[0];
        let iterator = match &args[1] {
            Expression::IteratorExpression(i) => i,
            _ => return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(1),
            }),
        };

        let (accumulator_name, accumulator) = match self
            .evaluator
            .evaluate_assignment(context, initializer)
            .await {
                Ok((name, value)) => (name, value),
                Err(e) => return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: e,
                }),
            };

        let mut query_variables = context.clone_variables(); //Retrieve the query variables from the global context
        query_variables.insert(accumulator_name.to_string().into(), accumulator);

        let context = ExpressionEvaluationContext::new(&query_variables, context.get_clock());
        let result = match self
            .evaluator
            .reduce_iterator_expression(&context, iterator, accumulator_name)
            .await {
                Ok(value) => value,
                Err(_e) => return Err(FunctionError {
                    function_name: expression.name.to_string(),
                    error: FunctionEvaluationError::InvalidType {
                        expected: "Valid reduce expression".to_string(),
                    },
                }),
            };

        Ok(result)
    }
}

impl std::fmt::Debug for Reduce {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reduce")
    }
}
