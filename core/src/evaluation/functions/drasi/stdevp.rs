use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{FunctionError, FunctionEvaluationError};
use async_trait::async_trait;
use drasi_query_ast::ast;
use statistical::{mean, population_standard_deviation};

use crate::evaluation::{variable_value::VariableValue, ExpressionEvaluationContext};

#[derive(Clone)]
pub struct DrasiStdevP {}

//NOTE: Do we want this to be an aggregating function instead?
#[async_trait]
impl ScalarFunction for DrasiStdevP {
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
            VariableValue::Null => Ok(VariableValue::Null),
            VariableValue::List(l) => {
                let mut cleaned_list = vec![];
                for element in l {
                    match element {
                        VariableValue::Integer(i) => {
                            cleaned_list.push(match i.as_i64() {
                                Some(i) => i as f64,
                                None => {
                                    return Err(FunctionError {
                                        function_name: expression.name.to_string(),
                                        error: FunctionEvaluationError::OverflowError,
                                    })
                                }
                            });
                        }
                        VariableValue::Float(f) => {
                            cleaned_list.push(match f.as_f64() {
                                Some(f) => f,
                                None => {
                                    return Err(FunctionError {
                                        function_name: expression.name.to_string(),
                                        error: FunctionEvaluationError::OverflowError,
                                    })
                                }
                            });
                        }
                        VariableValue::Null => {
                            continue;
                        }
                        _ => {
                            continue;
                        }
                    }
                }
                let mean = mean(&cleaned_list);
                let stdevp = population_standard_deviation(&cleaned_list, Some(mean));

                Ok(VariableValue::Float(stdevp.into()))
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
