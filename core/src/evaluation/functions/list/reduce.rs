use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use async_trait::async_trait;
use std::sync::Arc;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator};
use drasi_query_ast::ast::{self, Expression, ParentExpression, UnaryExpression};

#[derive(Debug)]
pub struct Reduce {}

#[async_trait]
impl ScalarFunction for Reduce {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 2 {
            return Err(EvaluationError::InvalidArgumentCount("reduce".to_string()));
        }
        let function_registry = Arc::new(FunctionRegistry::new());
        let ari = Arc::new(InMemoryResultIndex::new());
        let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

        let mut query_variables = context.clone_variables(); //Retrieve the query variables from the global context

        match (&args[0], &args[1]) {
            (
                VariableValue::Expression(accumulator),
                VariableValue::Expression(iteration_pattern),
            ) => {
                let accumulator_object = evaluator
                    .evaluate_expression(context, &accumulator.clone())
                    .await
                    .unwrap();
                let accumulator_map = accumulator_object.as_object().unwrap();

                let in_expression = iteration_pattern.get_children()[0]; //a IN B | .... -> a IN B

                let expression = iteration_pattern.get_children()[1]; //a IN B | .... -> .... (actual expression)
                let list_expression = in_expression.get_children()[1]; //a IN B -> B (list of literal values)
                let variable = match in_expression.get_children()[0] {
                    Expression::UnaryExpression(exp) => match exp {
                        UnaryExpression::Identifier(ident) => ident,
                        _ => return Err(EvaluationError::InvalidType),
                    },
                    _ => return Err(EvaluationError::InvalidType),
                };

                let list_evaluated = evaluator
                    .evaluate_expression(context, &list_expression.clone())
                    .await
                    .unwrap();
                let val_list = match list_evaluated {
                    VariableValue::List(list) => list,
                    _ => return Err(EvaluationError::InvalidType),
                };
                // Evaluate the list, converts Literal values into VariableValues
                //vec![Litearl(1), Literal(2), Literal(3)] -> vec![Integer(1), Integer(2), Integer(3)]

                let accumulator_name = accumulator_map.keys().next().unwrap(); //acc
                let accumulator_value = accumulator_map.get(accumulator_name).unwrap(); // 0

                query_variables.insert(accumulator_name.clone().into(), accumulator_value.clone());

                for value in &val_list {
                    query_variables.insert(variable.to_string().into_boxed_str(), value.clone());
                    let context =
                        ExpressionEvaluationContext::new(&query_variables, context.get_clock());
                    let result = evaluator
                        .evaluate_expression(&context, &expression.clone())
                        .await
                        .unwrap();

                    query_variables.insert(accumulator_name.clone().into(), result.clone());
                }

                Ok(query_variables
                    .get(accumulator_name.as_str())
                    .unwrap()
                    .clone())
            }
            _ => return Err(EvaluationError::InvalidType),
        }
    }
}
