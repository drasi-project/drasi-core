use std::sync::Arc;

use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::{context::SideEffects, ExpressionEvaluationContext, FunctionError};

use super::ContextMutatorFunction;

use super::{Function, FunctionRegistry};

pub trait RegisterContextMutatorFunctions {
    fn register_context_mutators(&self);
}

impl RegisterContextMutatorFunctions for FunctionRegistry {
    fn register_context_mutators(&self) {
        self.register_function(
            "retainHistory",
            Function::ContextMutator(Arc::new(RetainHistory {})),
        );
    }
}

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
