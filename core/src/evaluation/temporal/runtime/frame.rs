// Copyright 2026 The Drasi Authors.
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

use crate::evaluation::QueryExecutionError;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use drasi_query_ast::ast::FunctionExpression;

use crate::evaluation::{
    functions::FunctionEffect, variable_value::VariableValue, EvaluationError,
    ExpressionEvaluationContext,
};

use super::super::{
    ClockStamp, ContributionKey, FunctionCall, FunctionSite, SavedContext, TemporalInputId,
};

#[derive(Debug, Clone)]
pub struct CapturedCall {
    pub call: FunctionCall,
    pub expression: FunctionExpression,
    pub effect: FunctionEffect,
    pub arguments: Vec<VariableValue>,
    pub key: ContributionKey,
    pub context: SavedContext,
}

/// One input's progress through a part event. A stage captures without mutating accumulators;
/// only the part coordinator settles it after every affected input has reached the barrier.
#[derive(Debug, Default)]
pub struct EvaluationFrame {
    pub target: Option<FunctionSite>,
    pub settled: BTreeSet<FunctionSite>,
    pub captured: BTreeMap<FunctionCall, CapturedCall>,
    pub values: BTreeMap<FunctionCall, VariableValue>,
}

#[derive(Debug, Clone)]
pub struct TemporalEvaluation {
    pub input: TemporalInputId,
    pub occurrence: Vec<u32>,
    pub frame: Arc<Mutex<EvaluationFrame>>,
}

impl TemporalEvaluation {
    pub fn call(&self, expression: &FunctionExpression) -> FunctionCall {
        FunctionCall {
            site: FunctionSite {
                part: self.input.part,
                position_in_query: expression.position_in_query,
            },
            occurrence: self.occurrence.clone(),
        }
    }

    pub fn enter_iteration(&mut self, ordinal: usize) -> Result<(), EvaluationError> {
        self.occurrence
            .push(u32::try_from(ordinal).map_err(|_| EvaluationError::OverflowError)?);
        Ok(())
    }

    pub fn is_settled(&self, call: &FunctionCall) -> Result<bool, EvaluationError> {
        Ok(self
            .frame
            .lock()
            .map_err(|_| EvaluationError::CorruptData)?
            .settled
            .contains(&call.site))
    }

    pub fn value(&self, call: &FunctionCall) -> Result<VariableValue, EvaluationError> {
        self.frame
            .lock()
            .map_err(|_| EvaluationError::CorruptData)?
            .values
            .get(call)
            .cloned()
            .ok_or(EvaluationError::CorruptData)
    }

    pub fn capture(
        &self,
        expression: &FunctionExpression,
        effect: FunctionEffect,
        arguments: Vec<VariableValue>,
        key: ContributionKey,
        context: &ExpressionEvaluationContext<'_>,
    ) -> Result<VariableValue, EvaluationError> {
        let call = self.call(expression);
        let mut frame = self
            .frame
            .lock()
            .map_err(|_| EvaluationError::CorruptData)?;
        if frame.target.as_ref() == Some(&call.site) {
            let saved = SavedContext {
                clock: ClockStamp {
                    transaction_time: context.get_transaction_time(),
                    realtime: context.get_realtime(),
                },
                input_grouping_hash: context.get_input_grouping_hash(),
                solution_signature: context.get_solution_signature(),
                anchor: context.get_anchor_element(),
            };
            if frame
                .captured
                .insert(
                    call.clone(),
                    CapturedCall {
                        call,
                        expression: expression.clone(),
                        effect,
                        arguments,
                        key,
                        context: saved,
                    },
                )
                .is_some()
            {
                return Err(EvaluationError::InvalidContext);
            }
        }
        Err(EvaluationError::from(QueryExecutionError::TemporalDeferred))
    }
}
