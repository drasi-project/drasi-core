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

use chrono::NaiveTime;

use crate::{
    evaluation::{
        functions::FunctionEffect,
        temporal::{
            FunctionCall, FunctionCell, FunctionCellId, FunctionState, Generation, HistoryCapture,
            HistoryState, PredicateState, RetainedInput, TemporalStateError,
        },
        variable_value::VariableValue,
        EvaluationError, FunctionError, FunctionEvaluationError,
    },
    models::ElementReference,
};

use super::frame::CapturedCall;

#[derive(Debug, Clone)]
pub struct RequestedTicket {
    pub call: FunctionCall,
    pub slot: u32,
    pub cell: Option<FunctionCellId>,
    pub activation: Generation,
    pub original_time: u64,
    pub due_time: u64,
    pub attribution: Option<ElementReference>,
}

#[derive(Debug, Clone)]
pub struct SettledFunction {
    pub value: VariableValue,
    pub tickets: Vec<RequestedTicket>,
}

pub fn settle_function(
    captured: &CapturedCall,
    input: &mut RetainedInput,
    cell: Option<&mut FunctionCell>,
    capture_history: bool,
) -> Result<SettledFunction, EvaluationError> {
    match captured.effect {
        FunctionEffect::TrueFor => settle_true_for(captured, input, cell),
        FunctionEffect::PreviousValue | FunctionEffect::PreviousDistinctValue => settle_history(
            captured,
            input,
            cell,
            capture_history,
            captured.effect == FunctionEffect::PreviousDistinctValue,
        ),
        FunctionEffect::SlidingWindow => settle_window(captured, input),
        FunctionEffect::Future
        | FunctionEffect::TrueLater
        | FunctionEffect::TrueUntil
        | FunctionEffect::TrueNowOrLater => settle_at(captured, input, captured.effect),
        FunctionEffect::Pure | FunctionEffect::Aggregate => Err(EvaluationError::from(
            QueryExecutionError::UnsupportedTemporalEffect("non-temporal function settlement"),
        )),
    }
}

fn settle_at(
    captured: &CapturedCall,
    input: &RetainedInput,
    function: FunctionEffect,
) -> Result<SettledFunction, EvaluationError> {
    require_arguments(captured, 2)?;
    let value = &captured.arguments[0];
    if function == FunctionEffect::Future {
        let VariableValue::Element(element) = value else {
            return Err(function_error(
                captured,
                FunctionEvaluationError::InvalidArgument(0),
            ));
        };
        let due_time = deadline(captured, &captured.arguments[1])?;
        if captured.context.clock.realtime >= due_time {
            return Ok(settled(value.clone()));
        }
        let mut ticket = request(captured, input, due_time);
        ticket.attribution = Some(element.get_reference().clone());
        return Ok(SettledFunction {
            value: VariableValue::Awaiting,
            tickets: vec![ticket],
        });
    }

    let condition = match value {
        VariableValue::Bool(condition) => *condition,
        VariableValue::Null => return Ok(settled(VariableValue::Null)),
        _ => {
            return Err(function_error(
                captured,
                FunctionEvaluationError::InvalidArgument(0),
            ));
        }
    };
    if function == FunctionEffect::TrueNowOrLater && condition {
        return Ok(settled(VariableValue::Bool(true)));
    }
    if captured.arguments[1] == VariableValue::Null {
        return Ok(settled(VariableValue::Null));
    }
    let due_time = deadline(captured, &captured.arguments[1])?;
    // trueUntil keeps its existing meaning: false is immediate, true waits for the deadline.
    if (function == FunctionEffect::TrueUntil && !condition)
        || captured.context.clock.realtime >= due_time
    {
        return Ok(settled(VariableValue::Bool(condition)));
    }
    Ok(SettledFunction {
        value: VariableValue::Awaiting,
        tickets: vec![request(captured, input, due_time)],
    })
}

fn settle_true_for(
    captured: &CapturedCall,
    input: &RetainedInput,
    cell: Option<&mut FunctionCell>,
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
            let due_time = since
                .checked_add(duration)
                .ok_or_else(|| function_error(captured, FunctionEvaluationError::OverflowError))?;
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

fn settle_history(
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

fn settle_window(
    captured: &CapturedCall,
    input: &RetainedInput,
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

fn deadline(captured: &CapturedCall, value: &VariableValue) -> Result<u64, EvaluationError> {
    let millis = match value {
        VariableValue::Date(date) => date.and_time(NaiveTime::MIN).and_utc().timestamp_millis(),
        VariableValue::LocalDateTime(datetime) => datetime.and_utc().timestamp_millis(),
        VariableValue::ZonedDateTime(datetime) => datetime.datetime().timestamp_millis(),
        VariableValue::Integer(integer) => {
            return integer
                .as_u64()
                .ok_or_else(|| function_error(captured, FunctionEvaluationError::OverflowError));
        }
        _ => {
            return Err(function_error(
                captured,
                FunctionEvaluationError::InvalidArgument(1),
            ));
        }
    };
    u64::try_from(millis)
        .map_err(|_| function_error(captured, FunctionEvaluationError::OverflowError))
}

fn duration_millis(
    captured: &CapturedCall,
    value: &VariableValue,
    argument: usize,
) -> Result<Option<u64>, EvaluationError> {
    let millis = match value {
        VariableValue::Duration(duration) => {
            if *duration.duration() < chrono::Duration::zero() {
                return Err(function_error(
                    captured,
                    FunctionEvaluationError::OverflowError,
                ));
            }
            duration.duration().num_milliseconds()
        }
        VariableValue::Integer(integer) => integer
            .as_i64()
            .ok_or_else(|| function_error(captured, FunctionEvaluationError::OverflowError))?,
        VariableValue::Null => return Ok(None),
        _ => {
            return Err(function_error(
                captured,
                FunctionEvaluationError::InvalidArgument(argument),
            ));
        }
    };
    u64::try_from(millis)
        .map(Some)
        .map_err(|_| function_error(captured, FunctionEvaluationError::OverflowError))
}

fn request(captured: &CapturedCall, input: &RetainedInput, due_time: u64) -> RequestedTicket {
    RequestedTicket {
        call: captured.call.clone(),
        slot: 0,
        cell: None,
        activation: Generation(0),
        original_time: input.source_clock.transaction_time,
        due_time,
        attribution: captured
            .context
            .anchor
            .as_ref()
            .map(|element| element.get_reference().clone()),
    }
}

fn settled(value: VariableValue) -> SettledFunction {
    SettledFunction {
        value,
        tickets: Vec::new(),
    }
}

fn require_arguments(captured: &CapturedCall, count: usize) -> Result<(), EvaluationError> {
    if captured.arguments.len() == count {
        Ok(())
    } else {
        Err(function_error(
            captured,
            FunctionEvaluationError::InvalidArgumentCount,
        ))
    }
}

fn function_error(captured: &CapturedCall, error: FunctionEvaluationError) -> EvaluationError {
    EvaluationError::FunctionError(FunctionError {
        function_name: captured.expression.name.to_string(),
        error,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use chrono::{DateTime, NaiveDate};
    use drasi_query_ast::ast::FunctionExpression;

    use super::*;
    use crate::{
        evaluation::{
            temporal::{
                codec, fixtures, ContributionKey, Incarnation, SourceRevision, StateOwner,
                TemporalGroupId, TemporalRecord, TrueForState,
            },
            variable_value::{duration::Duration, float::Float, zoned_datetime::ZonedDateTime},
        },
        models::{Element, ElementMetadata},
    };

    fn capture(
        function: FunctionEffect,
        arguments: Vec<VariableValue>,
        realtime: u64,
    ) -> CapturedCall {
        let mut context = fixtures::input().context;
        context.clock.realtime = realtime;
        CapturedCall {
            call: fixtures::call(),
            expression: FunctionExpression {
                name: format!("{function:?}").into(),
                args: Vec::new(),
                position_in_query: fixtures::call().site.position_in_query,
            },
            effect: function,
            arguments,
            key: ContributionKey::InputHash(13),
            context,
        }
    }

    fn cell(state: FunctionState) -> FunctionCell {
        let input = fixtures::input();
        FunctionCell {
            id: FunctionCellId {
                owner: StateOwner::Group(TemporalGroupId {
                    namespace: input.id.namespace,
                    producer: input.id.part,
                    incarnation: Incarnation(42),
                }),
                call: fixtures::call(),
            },
            state,
            subscribers: BTreeSet::from([input.id]),
        }
    }

    fn element(id: &str) -> Arc<Element> {
        Arc::new(Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::from([Arc::from("Native")]),
                effective_from: 123,
            },
            properties: Default::default(),
        })
    }

    fn native_value() -> VariableValue {
        VariableValue::List(vec![
            VariableValue::Float(Float::from(f64::from_bits(0x7ff8_0000_0000_1234))),
            VariableValue::Float(Float::from(-0.0)),
            VariableValue::Duration(Duration::new(
                chrono::Duration::nanoseconds(1_234_567_891),
                -2,
                17,
            )),
            VariableValue::Awaiting,
            VariableValue::Element(element("history")),
        ])
    }

    fn assert_native(actual: &VariableValue, expected: &VariableValue) {
        assert_eq!(
            codec::encode_value(actual).unwrap(),
            codec::encode_value(expected).unwrap()
        );
    }

    fn cell_bytes(cell: &FunctionCell) -> Vec<u8> {
        codec::encode_record(&TemporalRecord::Cell(cell.clone())).unwrap()
    }

    fn input_bytes(input: &RetainedInput) -> Vec<u8> {
        codec::encode_record(&TemporalRecord::Input(input.clone())).unwrap()
    }

    fn assert_error(
        result: Result<SettledFunction, EvaluationError>,
        expected: FunctionEvaluationError,
    ) {
        let Err(EvaluationError::FunctionError(error)) = result else {
            panic!("expected {expected:?}, got {result:?}");
        };
        assert_eq!(error.error, expected);
    }

    #[test]
    fn true_for_preserves_net_true_start_and_activation_until_false() {
        let mut input = fixtures::input();
        let mut cell = cell(FunctionState::TrueFor(TrueForState::default()));
        let mut captured = capture(
            FunctionEffect::TrueFor,
            vec![VariableValue::Bool(true), VariableValue::from(20)],
            110,
        );
        let first = settle_function(&captured, &mut input, Some(&mut cell), true).unwrap();
        assert_eq!(first.value, VariableValue::Awaiting);
        assert_eq!(first.tickets.len(), 1);
        assert_eq!(first.tickets[0].due_time, 130);
        assert_eq!(first.tickets[0].original_time, 100);
        assert_eq!(first.tickets[0].cell, Some(cell.id.clone()));
        assert_eq!(first.tickets[0].activation, Generation(0));
        let active = cell_bytes(&cell);
        captured.context.clock.realtime = 119;
        for _ in 0..2 {
            let waiting = settle_function(&captured, &mut input, Some(&mut cell), false).unwrap();
            assert_eq!(waiting.value, VariableValue::Awaiting);
            assert_eq!(waiting.tickets[0].due_time, 130);
            assert_eq!(cell_bytes(&cell), active);
        }
        captured.context.clock.realtime = 130;
        let matured = settle_function(&captured, &mut input, Some(&mut cell), false).unwrap();
        assert_eq!(matured.value, VariableValue::Bool(true));
        assert!(matured.tickets.is_empty());
        assert_eq!(cell_bytes(&cell), active);

        captured.arguments[0] = VariableValue::Bool(false);
        let stopped = settle_function(&captured, &mut input, Some(&mut cell), true).unwrap();
        assert_eq!(stopped.value, VariableValue::Bool(false));
        assert!(stopped.tickets.is_empty());
        captured.arguments[0] = VariableValue::Bool(true);
        captured.context.clock.realtime = 131;
        let restarted = settle_function(&captured, &mut input, Some(&mut cell), true).unwrap();
        assert_eq!(restarted.tickets[0].activation, Generation(1));
        assert_eq!(restarted.tickets[0].due_time, 151);
        assert!(matches!(
            cell.state,
            FunctionState::TrueFor(TrueForState {
                predicate: PredicateState::True {
                    since: 131,
                    activation: Generation(1)
                },
                next_activation: Generation(2),
            })
        ));
        assert!(input.tickets.is_empty());
        assert_eq!(input.next_ticket_generation, Generation(0));
    }

    #[test]
    fn true_for_null_and_zero_duration_preserve_existing_results() {
        let mut input = fixtures::input();
        let mut cell = cell(FunctionState::TrueFor(TrueForState::default()));
        for args in [
            vec![VariableValue::Null, VariableValue::from("ignored")],
            vec![VariableValue::Bool(true), VariableValue::Null],
        ] {
            let captured = capture(FunctionEffect::TrueFor, args, 110);
            let before = cell_bytes(&cell);
            let result = settle_function(&captured, &mut input, Some(&mut cell), true).unwrap();
            assert_eq!(result.value, VariableValue::Null);
            assert!(result.tickets.is_empty());
            assert_eq!(cell_bytes(&cell), before);
        }
        let captured = capture(
            FunctionEffect::TrueFor,
            vec![VariableValue::Bool(true), VariableValue::from(0)],
            110,
        );
        let result = settle_function(&captured, &mut input, Some(&mut cell), true).unwrap();
        assert_eq!(result.value, VariableValue::Bool(true));
        assert!(result.tickets.is_empty());
    }

    #[test]
    fn shared_history_advances_once_per_revision_without_losing_native_values() {
        for function in [
            FunctionEffect::PreviousValue,
            FunctionEffect::PreviousDistinctValue,
        ] {
            let mut first = fixtures::input();
            first.source_revision = SourceRevision(0);
            let mut second = first.clone();
            second.id.incarnation = Incarnation(1);
            let mut cell = cell(FunctionState::UninitializedHistory);
            let initial = native_value();
            let default = VariableValue::Awaiting;
            let mut captured = capture(function, vec![initial.clone(), default.clone()], 110);
            for input in [&mut first, &mut second] {
                let result = settle_function(&captured, input, Some(&mut cell), true).unwrap();
                assert_native(&result.value, &default);
                assert!(result.tickets.is_empty());
                assert_eq!(input.history.len(), 1);
            }

            first.source_revision = SourceRevision(1);
            second.source_revision = SourceRevision(1);
            captured.arguments[0] = VariableValue::from("current");
            let result = settle_function(&captured, &mut first, Some(&mut cell), true).unwrap();
            assert_native(&result.value, &initial);
            let once = cell_bytes(&cell);
            captured.arguments[0] = VariableValue::from("another subscriber");
            for input in [&mut second, &mut first] {
                let result = settle_function(&captured, input, Some(&mut cell), true).unwrap();
                assert_native(&result.value, &initial);
                assert_native(&input.history[&captured.call].value, &initial);
                assert_eq!(input.history.len(), 1);
                assert_eq!(cell_bytes(&cell), once);
            }
            let FunctionState::History(history) = &cell.state else {
                panic!("history was not initialized");
            };
            assert_eq!(history.revision, SourceRevision(1));
            assert_eq!(history.current, VariableValue::from("current"));
            assert_native(&history.previous, &initial);
        }
    }

    #[test]
    fn history_refresh_uses_input_capture_and_never_writes_history() {
        for function in [
            FunctionEffect::PreviousValue,
            FunctionEffect::PreviousDistinctValue,
        ] {
            let mut input = fixtures::input();
            let saved = native_value();
            input.history.insert(
                fixtures::call(),
                HistoryCapture {
                    revision: input.source_revision,
                    value: saved.clone(),
                },
            );
            let mut cell = cell(FunctionState::History(HistoryState {
                revision: SourceRevision(input.source_revision.0 + 1),
                current: VariableValue::from("new current"),
                previous: VariableValue::from("shared previous"),
            }));
            let captured = capture(function, vec![VariableValue::from("must not capture")], 999);
            let before_input = input_bytes(&input);
            let before_cell = cell_bytes(&cell);
            let result = settle_function(&captured, &mut input, Some(&mut cell), false).unwrap();
            assert_native(&result.value, &saved);
            assert_eq!(input_bytes(&input), before_input);
            assert_eq!(cell_bytes(&cell), before_cell);

            input.history.clear();
            let before_input = input_bytes(&input);
            let result = settle_function(&captured, &mut input, Some(&mut cell), false).unwrap();
            assert_eq!(result.value, VariableValue::from("shared previous"));
            assert_eq!(input_bytes(&input), before_input);
            assert_eq!(cell_bytes(&cell), before_cell);

            cell.state = FunctionState::UninitializedHistory;
            let captured = capture(function, vec![VariableValue::Null, saved.clone()], 999);
            let before_cell = cell_bytes(&cell);
            let result = settle_function(&captured, &mut input, Some(&mut cell), false).unwrap();
            assert_native(&result.value, &saved);
            assert_eq!(input_bytes(&input), before_input);
            assert_eq!(cell_bytes(&cell), before_cell);
        }
    }

    #[test]
    fn previous_distinct_value_keeps_previous_across_equal_values_and_captures_null() {
        let mut input = fixtures::input();
        let mut cell = cell(FunctionState::UninitializedHistory);
        for (revision, current, previous) in [
            (0, VariableValue::from("a"), VariableValue::Null),
            (1, VariableValue::from("b"), VariableValue::from("a")),
            (2, VariableValue::from("b"), VariableValue::from("a")),
            (3, VariableValue::Null, VariableValue::from("b")),
            (4, VariableValue::Null, VariableValue::from("b")),
            (5, VariableValue::from("b"), VariableValue::Null),
        ] {
            input.source_revision = SourceRevision(revision);
            let captured = capture(FunctionEffect::PreviousDistinctValue, vec![current], 110);
            let result = settle_function(&captured, &mut input, Some(&mut cell), true).unwrap();
            assert_eq!(result.value, previous);
        }
    }

    #[test]
    fn missing_wrong_and_regressing_history_state_are_errors() {
        let mut input = fixtures::input();
        for function in [
            FunctionEffect::TrueFor,
            FunctionEffect::PreviousValue,
            FunctionEffect::PreviousDistinctValue,
        ] {
            let captured = capture(
                function,
                vec![VariableValue::Bool(true), VariableValue::from(10)],
                110,
            );
            assert_error(
                settle_function(&captured, &mut input, None, true),
                FunctionEvaluationError::CorruptData,
            );
            let wrong = if function == FunctionEffect::TrueFor {
                FunctionState::UninitializedHistory
            } else {
                FunctionState::TrueFor(TrueForState::default())
            };
            assert_error(
                settle_function(&captured, &mut input, Some(&mut cell(wrong)), true),
                FunctionEvaluationError::CorruptData,
            );
        }
        let mut cell = cell(FunctionState::History(HistoryState {
            revision: SourceRevision(5),
            current: VariableValue::Null,
            previous: VariableValue::Null,
        }));
        let captured = capture(
            FunctionEffect::PreviousValue,
            vec![VariableValue::Null],
            110,
        );
        let before = cell_bytes(&cell);
        assert_error(
            settle_function(&captured, &mut input, Some(&mut cell), true),
            FunctionEvaluationError::CorruptData,
        );
        assert_eq!(cell_bytes(&cell), before);
        assert!(input.history.is_empty());
    }

    #[test]
    fn later_values_wait_until_the_deadline_without_an_anchor() {
        for (function, condition) in [
            (FunctionEffect::TrueLater, true),
            (FunctionEffect::TrueLater, false),
            (FunctionEffect::TrueUntil, true),
            (FunctionEffect::TrueNowOrLater, false),
        ] {
            let mut input = fixtures::input();
            let mut captured = capture(
                function,
                vec![VariableValue::Bool(condition), VariableValue::from(200)],
                199,
            );
            let before = input_bytes(&input);
            let waiting = settle_function(&captured, &mut input, None, true).unwrap();
            assert_eq!(waiting.value, VariableValue::Awaiting);
            assert_eq!(waiting.tickets.len(), 1);
            let ticket = &waiting.tickets[0];
            assert_eq!(ticket.call, captured.call);
            assert_eq!(ticket.slot, 0);
            assert_eq!(ticket.due_time, 200);
            assert!(ticket.due_time > captured.context.clock.realtime);
            assert_eq!(ticket.original_time, input.source_clock.transaction_time);
            assert!(ticket.cell.is_none());
            assert!(ticket.attribution.is_none());
            for realtime in [200, 201] {
                captured.context.clock.realtime = realtime;
                let ready = settle_function(&captured, &mut input, None, false).unwrap();
                assert_eq!(ready.value, VariableValue::Bool(condition));
                assert!(ready.tickets.is_empty());
            }
            assert_eq!(input_bytes(&input), before);
        }
    }

    #[test]
    fn until_and_now_or_later_keep_their_short_circuits() {
        let mut input = fixtures::input();
        for (function, condition, due, expected) in [
            (
                FunctionEffect::TrueUntil,
                false,
                VariableValue::from(200),
                VariableValue::Bool(false),
            ),
            (
                FunctionEffect::TrueUntil,
                false,
                VariableValue::Null,
                VariableValue::Null,
            ),
            (
                FunctionEffect::TrueNowOrLater,
                true,
                VariableValue::from("not a deadline"),
                VariableValue::Bool(true),
            ),
        ] {
            let captured = capture(function, vec![VariableValue::Bool(condition), due], 110);
            let result = settle_function(&captured, &mut input, None, true).unwrap();
            assert_eq!(result.value, expected);
            assert!(result.tickets.is_empty());
        }
        for function in [
            FunctionEffect::TrueLater,
            FunctionEffect::TrueUntil,
            FunctionEffect::TrueNowOrLater,
        ] {
            let captured = capture(
                function,
                vec![VariableValue::Null, VariableValue::from("ignored")],
                110,
            );
            let result = settle_function(&captured, &mut input, None, true).unwrap();
            assert_eq!(result.value, VariableValue::Null);
            assert!(result.tickets.is_empty());
        }
    }

    #[test]
    fn future_returns_native_element_and_attributes_its_explicit_reference() {
        let mut input = fixtures::input();
        let target = element("target");
        let mut captured = capture(
            FunctionEffect::Future,
            vec![
                VariableValue::Element(target.clone()),
                VariableValue::from(200),
            ],
            110,
        );
        captured.context.anchor = Some(element("anchor"));
        let waiting = settle_function(&captured, &mut input, None, true).unwrap();
        assert_eq!(waiting.value, VariableValue::Awaiting);
        assert_eq!(
            waiting.tickets[0].attribution.as_ref(),
            Some(target.get_reference())
        );
        captured.context.clock.realtime = 200;
        let ready = settle_function(&captured, &mut input, None, false).unwrap();
        let VariableValue::Element(restored) = ready.value else {
            panic!("future lost its native element");
        };
        assert!(Arc::ptr_eq(&restored, &target));
        assert!(ready.tickets.is_empty());
    }

    #[test]
    fn deadlines_accept_native_temporal_types_and_full_unsigned_range() {
        let mut input = fixtures::input();
        let date = NaiveDate::from_ymd_opt(1970, 1, 2).unwrap();
        for (deadline, expected) in [
            (VariableValue::Date(date), 86_400_000),
            (
                VariableValue::LocalDateTime(date.and_hms_milli_opt(0, 0, 0, 123).unwrap()),
                86_400_123,
            ),
            (
                VariableValue::ZonedDateTime(ZonedDateTime::new(
                    DateTime::from_timestamp_millis(999).unwrap().fixed_offset(),
                    None,
                )),
                999,
            ),
            (VariableValue::from(u64::MAX), u64::MAX),
        ] {
            let captured = capture(
                FunctionEffect::TrueLater,
                vec![VariableValue::Bool(true), deadline],
                110,
            );
            let result = settle_function(&captured, &mut input, None, true).unwrap();
            assert_eq!(result.tickets[0].due_time, expected);
        }
    }

    #[test]
    fn negative_deadlines_and_durations_return_function_overflow() {
        let mut input = fixtures::input();
        for deadline in [
            VariableValue::from(-1),
            VariableValue::Date(NaiveDate::MIN),
            VariableValue::LocalDateTime(
                NaiveDate::from_ymd_opt(1969, 12, 31)
                    .unwrap()
                    .and_hms_nano_opt(23, 59, 59, 999_999_999)
                    .unwrap(),
            ),
            VariableValue::ZonedDateTime(ZonedDateTime::new(
                DateTime::from_timestamp_millis(-1).unwrap().fixed_offset(),
                None,
            )),
        ] {
            let captured = capture(
                FunctionEffect::TrueLater,
                vec![VariableValue::Bool(true), deadline],
                110,
            );
            assert_error(
                settle_function(&captured, &mut input, None, true),
                FunctionEvaluationError::OverflowError,
            );
        }
        for duration in [
            VariableValue::from(-1),
            VariableValue::from(i64::MIN),
            VariableValue::from(u64::MAX),
            VariableValue::Duration(Duration::new(chrono::Duration::nanoseconds(-1), 0, 0)),
        ] {
            for function in [FunctionEffect::TrueFor, FunctionEffect::SlidingWindow] {
                let args = if function == FunctionEffect::TrueFor {
                    vec![VariableValue::Bool(true), duration.clone()]
                } else {
                    vec![duration.clone()]
                };
                let captured = capture(function, args, 110);
                let mut cell = cell(FunctionState::TrueFor(TrueForState::default()));
                let before = cell_bytes(&cell);
                assert_error(
                    settle_function(&captured, &mut input, Some(&mut cell), true),
                    FunctionEvaluationError::OverflowError,
                );
                assert_eq!(cell_bytes(&cell), before);
            }
        }
    }

    #[test]
    fn true_for_checks_deadline_and_activation_overflow_before_mutation() {
        let mut input = fixtures::input();
        for (realtime, state) in [
            (u64::MAX, TrueForState::default()),
            (
                110,
                TrueForState {
                    predicate: PredicateState::False,
                    next_activation: Generation(u64::MAX),
                },
            ),
        ] {
            let captured = capture(
                FunctionEffect::TrueFor,
                vec![VariableValue::Bool(true), VariableValue::from(1)],
                realtime,
            );
            let mut cell = cell(FunctionState::TrueFor(state));
            let before = cell_bytes(&cell);
            assert_error(
                settle_function(&captured, &mut input, Some(&mut cell), true),
                FunctionEvaluationError::OverflowError,
            );
            assert_eq!(cell_bytes(&cell), before);
        }
    }

    #[test]
    fn sliding_window_uses_source_time_and_stops_scheduling_when_expired() {
        let mut input = fixtures::input();
        let mut captured = capture(
            FunctionEffect::SlidingWindow,
            vec![VariableValue::Duration(Duration::new(
                chrono::Duration::milliseconds(20),
                0,
                0,
            ))],
            110,
        );
        captured.context.clock.transaction_time = 1_000;
        let before = input_bytes(&input);
        let pending = settle_function(&captured, &mut input, None, true).unwrap();
        assert_eq!(pending.value, VariableValue::Bool(false));
        assert_eq!(pending.tickets.len(), 1);
        assert_eq!(pending.tickets[0].due_time, 120);
        assert_eq!(pending.tickets[0].original_time, 100);
        assert!(pending.tickets[0].cell.is_none());
        for realtime in [120, 120, 121] {
            captured.context.clock.realtime = realtime;
            let expired = settle_function(&captured, &mut input, None, false).unwrap();
            assert_eq!(expired.value, VariableValue::Bool(true));
            assert!(expired.tickets.is_empty());
            assert_eq!(input_bytes(&input), before);
        }
        input.source_clock.transaction_time = u64::MAX;
        assert_error(
            settle_function(&captured, &mut input, None, true),
            FunctionEvaluationError::OverflowError,
        );
    }

    #[test]
    fn argument_validation_is_function_scoped_and_non_temporal_effects_are_rejected() {
        let mut input = fixtures::input();
        for (function, args, error) in [
            (
                FunctionEffect::TrueLater,
                vec![],
                FunctionEvaluationError::InvalidArgumentCount,
            ),
            (
                FunctionEffect::TrueLater,
                vec![VariableValue::Awaiting, VariableValue::from(200)],
                FunctionEvaluationError::InvalidArgument(0),
            ),
            (
                FunctionEffect::TrueUntil,
                vec![VariableValue::Bool(false), VariableValue::from("bad time")],
                FunctionEvaluationError::InvalidArgument(1),
            ),
            (
                FunctionEffect::Future,
                vec![VariableValue::Null, VariableValue::from(200)],
                FunctionEvaluationError::InvalidArgument(0),
            ),
            (
                FunctionEffect::Future,
                vec![
                    VariableValue::Element(element("target")),
                    VariableValue::Null,
                ],
                FunctionEvaluationError::InvalidArgument(1),
            ),
            (
                FunctionEffect::SlidingWindow,
                vec![VariableValue::from("bad duration")],
                FunctionEvaluationError::InvalidArgument(0),
            ),
            (
                FunctionEffect::SlidingWindow,
                vec![
                    VariableValue::from(10),
                    VariableValue::from("lazy expression"),
                ],
                FunctionEvaluationError::InvalidArgumentCount,
            ),
            (
                FunctionEffect::PreviousValue,
                vec![],
                FunctionEvaluationError::InvalidArgumentCount,
            ),
        ] {
            let captured = capture(function, args, 110);
            assert_error(settle_function(&captured, &mut input, None, true), error);
        }
        let mut captured = capture(FunctionEffect::TrueLater, vec![], 110);
        captured.effect = FunctionEffect::Pure;
        assert!(matches!(
            settle_function(&captured, &mut input, None, true),
            Err(error) if matches!(error.execution_error(), Some(QueryExecutionError::UnsupportedTemporalEffect(_)))
        ));
    }
}
