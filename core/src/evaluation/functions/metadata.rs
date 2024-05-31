use std::sync::Arc;

use crate::evaluation::variable_value::{zoned_datetime::ZonedDateTime, VariableValue};
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};

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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                "elementId".to_string(),
            ));
        }
        match &args[0] {
            VariableValue::Element(e) => Ok(VariableValue::String(
                e.get_reference().element_id.to_string(),
            )),
            _ => Err(EvaluationError::InvalidType),
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
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                expression.name.to_string(),
            ));
        }
        match &args[0] {
            VariableValue::Element(e) => Ok(VariableValue::ZonedDateTime(
                ZonedDateTime::from_epoch_millis(e.get_effective_from()),
            )),
            _ => Err(EvaluationError::InvalidType),
        }
    }
}
