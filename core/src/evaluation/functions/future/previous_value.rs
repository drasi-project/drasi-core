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
use crate::evaluation::temporal::runtime::functions::{function_error, settled, SettledFunction};
use crate::evaluation::temporal::{
    FunctionCell, FunctionState, HistoryCapture, HistoryState, RetainedInput,
};
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, FunctionEvaluationError};

pub struct PreviousValue;

impl TemporalScalar for PreviousValue {
    fn initial_cell(&self) -> Option<FunctionState> {
        Some(FunctionState::UninitializedHistory)
    }

    fn accepts_cell(&self, state: &FunctionState) -> bool {
        matches!(
            state,
            FunctionState::UninitializedHistory | FunctionState::History(_)
        )
    }

    fn settle(
        &self,
        captured: &CapturedCall,
        input: &mut RetainedInput,
        cell: Option<&mut FunctionCell>,
        capture_history: bool,
    ) -> Result<SettledFunction, EvaluationError> {
        observe_history(captured, input, cell, capture_history, false)
    }
}

pub(crate) fn observe_history(
    captured: &CapturedCall,
    input: &mut RetainedInput,
    cell: Option<&mut FunctionCell>,
    capture_history: bool,
    distinct: bool,
) -> Result<SettledFunction, EvaluationError> {
    let Some(current) = captured.arguments.first() else {
        return Err(function_error(
            captured,
            FunctionEvaluationError::InvalidArgumentCount,
        ));
    };
    let default = captured.arguments.get(1).unwrap_or(&VariableValue::Null);
    let cell =
        cell.ok_or_else(|| function_error(captured, FunctionEvaluationError::CorruptData))?;
    let history = match &mut cell.state {
        FunctionState::History(history) => Some(history),
        FunctionState::UninitializedHistory => None,
        _ => {
            return Err(function_error(
                captured,
                FunctionEvaluationError::CorruptData,
            ))
        }
    };
    if let Some(saved) = input.history.get(&captured.call) {
        if saved.revision > input.source_revision {
            return Err(function_error(
                captured,
                FunctionEvaluationError::CorruptData,
            ));
        }
        if !capture_history || saved.revision == input.source_revision {
            return Ok(settled(saved.value.clone()));
        }
    }
    if !capture_history {
        return Ok(settled(
            history.map_or(default, |history| &history.previous).clone(),
        ));
    }
    let previous = match history {
        Some(history) => {
            if history.revision > input.source_revision {
                return Err(function_error(
                    captured,
                    FunctionEvaluationError::CorruptData,
                ));
            }
            if history.revision < input.source_revision {
                if !distinct || *current != history.current {
                    history.previous = history.current.clone();
                }
                history.current = current.clone();
                history.revision = input.source_revision;
            }
            history.previous.clone()
        }
        None => {
            cell.state = FunctionState::History(HistoryState {
                revision: input.source_revision,
                current: current.clone(),
                previous: default.clone(),
            });
            default.clone()
        }
    };
    input.history.insert(
        captured.call.clone(),
        HistoryCapture {
            revision: input.source_revision,
            value: previous.clone(),
        },
    );
    Ok(settled(previous))
}
