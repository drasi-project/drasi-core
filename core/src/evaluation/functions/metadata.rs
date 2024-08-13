use std::sync::Arc;

use crate::evaluation::{variable_value::{zoned_datetime::ZonedDateTime, VariableValue}, EvaluationError};
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext};

use super::{Function, FunctionRegistry, ScalarFunction};

pub trait RegisterMetadataFunctions {
    fn register_metadata_functions(&self);
}

impl RegisterMetadataFunctions for FunctionRegistry {
    fn register_metadata_functions(&self) {
        self.register_function("elementId", Function::Scalar(Arc::new(ElementId {})));
        self.register_function(
            "drasi.changeDateTime",
            Function::Scalar(Arc::new(ChangeDateTime {})),
        );
    }
}

#[derive(Debug)]
pub struct ElementId {}

#[async_trait]
impl ScalarFunction for ElementId {
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
            VariableValue::Element(e) => Ok(VariableValue::String(
                e.get_reference().element_id.to_string(),
            )),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct ChangeDateTime {}

#[async_trait]
impl ScalarFunction for ChangeDateTime {
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
            VariableValue::Element(e) => Ok(VariableValue::ZonedDateTime(
                match ZonedDateTime::from_epoch_millis(e.get_effective_from()) {
                    Ok(dt) => dt,
                    Err(e) => match e { 
                        EvaluationError::OverflowError => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            });
                        },
                        _ => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat { expected: "A valid DateTime component".to_string() },
                            });
                        }
                    }
                },
            )),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
