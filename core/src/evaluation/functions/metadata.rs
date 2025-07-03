// Copyright 2024 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use crate::evaluation::variable_value::{zoned_datetime::ZonedDateTime, VariableValue};
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

use super::{Function, FunctionRegistry, ScalarFunction};

pub trait RegisterCypherMetadataFunctions {
    fn register_cypher_metadata_functions(&self);
}

pub trait RegisterGqlMetadataFunctions {
    fn register_gql_metadata_functions(&self);
}

impl RegisterCypherMetadataFunctions for FunctionRegistry {
    fn register_cypher_metadata_functions(&self) {
        self.register_function("elementId", Function::Scalar(Arc::new(ElementId {})));
        self.register_function(
            "drasi.changeDateTime",
            Function::Scalar(Arc::new(ChangeDateTime {})),
        );
    }
}

impl RegisterGqlMetadataFunctions for FunctionRegistry {
    fn register_gql_metadata_functions(&self) {
        self.register_function("element_id", Function::Scalar(Arc::new(ElementId {})));
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
                ZonedDateTime::from_epoch_millis(e.get_effective_from() as i64),
            )),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}
