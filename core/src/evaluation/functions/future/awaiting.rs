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

use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{
    functions::ScalarFunction, variable_value::VariableValue, ExpressionEvaluationContext,
    FunctionError,
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
