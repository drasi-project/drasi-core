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

use super::previous_value::observe_history;
use crate::evaluation::functions::TemporalScalar;
use crate::evaluation::temporal::runtime::frame::CapturedCall;
use crate::evaluation::temporal::runtime::functions::SettledFunction;
use crate::evaluation::temporal::{FunctionCell, FunctionState, RetainedInput};
use crate::evaluation::EvaluationError;

pub struct PreviousDistinctValue;

impl TemporalScalar for PreviousDistinctValue {
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
        observe_history(captured, input, cell, capture_history, true)
    }
}
