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
use crate::evaluation::temporal::{FunctionCell, RetainedInput};
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, FunctionEvaluationError};

pub struct SlidingWindow;

impl TemporalScalar for SlidingWindow {
    fn settle(
        &self,
        captured: &CapturedCall,
        input: &mut RetainedInput,
        _cell: Option<&mut FunctionCell>,
        _capture_history: bool,
    ) -> Result<SettledFunction, EvaluationError> {
        require_arguments(captured, 1)?;
        let Some(duration) = duration_millis(captured, &captured.arguments[0], 0)? else {
            return Ok(settled(VariableValue::Null));
        };
        let due_time = input
            .source_clock
            .transaction_time
            .checked_add(duration)
            .ok_or_else(|| function_error(captured, FunctionEvaluationError::OverflowError))?;
        let expired = captured.context.clock.realtime >= due_time;
        Ok(SettledFunction {
            value: VariableValue::Bool(expired),
            tickets: if expired {
                Vec::new()
            } else {
                vec![request(captured, input, due_time)]
            },
        })
    }
}
