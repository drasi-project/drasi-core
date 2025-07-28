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

use crate::evaluation::{context::SideEffects, ExpressionEvaluationContext, FunctionError};

use super::ContextMutatorFunction;

pub struct RetainHistory {}

#[async_trait]
impl ContextMutatorFunction for RetainHistory {
    async fn call<'a>(
        &self,
        context: &ExpressionEvaluationContext<'a>,
        _expression: &ast::FunctionExpression,
    ) -> Result<ExpressionEvaluationContext<'a>, FunctionError> {
        let mut new_context = context.clone();
        match new_context.get_side_effects() {
            SideEffects::RevertForUpdate => new_context.set_side_effects(SideEffects::Snapshot),
            SideEffects::RevertForDelete => new_context.set_side_effects(SideEffects::Snapshot),
            _ => {}
        }

        Ok(new_context)
    }
}
