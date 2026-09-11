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

use crate::evaluation::functions::TemporalScalar;
use crate::evaluation::temporal::runtime::frame::CapturedCall;
use crate::evaluation::temporal::runtime::functions::{
    duration_millis, function_error, request, require_arguments, settled, SettledFunction,
};
use crate::evaluation::temporal::{
    FunctionCell, FunctionState, PredicateState, RetainedInput, TemporalStateError, TrueForState,
};
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, FunctionEvaluationError};

pub struct TrueFor;

impl TemporalScalar for TrueFor {
    fn initial_cell(&self) -> Option<FunctionState> {
        Some(FunctionState::TrueFor(TrueForState::default()))
    }

    fn release_cell(&self, cell: &mut FunctionCell) {
        if let FunctionState::TrueFor(state) = &mut cell.state {
            let _ = state.settle(false, 0);
        }
    }

    fn settle(
        &self,
        captured: &CapturedCall,
        input: &mut RetainedInput,
        cell: Option<&mut FunctionCell>,
        _capture_history: bool,
    ) -> Result<SettledFunction, EvaluationError> {
        require_arguments(captured, 2)?;
        let cell =
            cell.ok_or_else(|| function_error(captured, FunctionEvaluationError::CorruptData))?;
        let FunctionState::TrueFor(state) = &mut cell.state else {
            return Err(function_error(
                captured,
                FunctionEvaluationError::CorruptData,
            ));
        };
        let condition = match &captured.arguments[0] {
            VariableValue::Bool(condition) => *condition,
            VariableValue::Null => return Ok(settled(VariableValue::Null)),
            _ => {
                return Err(function_error(
                    captured,
                    FunctionEvaluationError::InvalidArgument(0),
                ));
            }
        };
        let Some(duration) = duration_millis(captured, &captured.arguments[1], 1)? else {
            return Ok(settled(VariableValue::Null));
        };
        let mut next = state.clone();
        next.settle(condition, captured.context.clock.realtime)
            .map_err(|error| {
                function_error(
                    captured,
                    match error {
                        TemporalStateError::GenerationExhausted => {
                            FunctionEvaluationError::OverflowError
                        }
                        _ => FunctionEvaluationError::CorruptData,
                    },
                )
            })?;
        let result = match &next.predicate {
            PredicateState::False => settled(VariableValue::Bool(false)),
            PredicateState::True { since, activation } => {
                let due_time = since.checked_add(duration).ok_or_else(|| {
                    function_error(captured, FunctionEvaluationError::OverflowError)
                })?;
                if captured.context.clock.realtime >= due_time {
                    settled(VariableValue::Bool(true))
                } else {
                    let mut ticket = request(captured, input, due_time);
                    ticket.cell = Some(cell.id.clone());
                    ticket.activation = *activation;
                    SettledFunction {
                        value: VariableValue::Awaiting,
                        tickets: vec![ticket],
                    }
                }
            }
        };
        *state = next;
        Ok(result)
    }
}
