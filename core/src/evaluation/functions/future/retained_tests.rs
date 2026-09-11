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

use chrono::NaiveDate;
use drasi_query_ast::ast;

use super::*;
use crate::{
    evaluation::{
        context::QueryVariables,
        functions::future::true_now_or_later::TrueNowOrLater,
        functions::{Function, TemporalScalar},
        temporal::{
            fixtures,
            runtime::{frame::CapturedCall, functions::SettledFunction},
            ClockStamp, ContributionKey, Generation, RetainedInput,
        },
        variable_value::VariableValue,
        EvaluationError, ExpressionEvaluationContext, FunctionEvaluationError, InstantQueryClock,
    },
    in_memory_index::{
        in_memory_future_queue::InMemoryFutureQueue, in_memory_result_index::InMemoryResultIndex,
    },
    models::{Element, ElementMetadata, ElementReference},
};

fn registry() -> FunctionRegistry {
    let registry = FunctionRegistry::new();
    registry.register_future_functions(
        Arc::new(InMemoryFutureQueue::new()),
        Arc::new(InMemoryResultIndex::new()),
        Weak::new(),
    );
    registry
}

const TEMPORAL: [&str; 7] = [
    "drasi.future",
    "drasi.trueUntil",
    "drasi.trueFor",
    "drasi.trueLater",
    "drasi.trueNowOrLater",
    "drasi.previousValue",
    "drasi.previousDistinctValue",
];

#[test]
fn temporal_declarations_use_temporal_variants() {
    let registry = registry();
    for name in TEMPORAL {
        let function = registry.get_function(name).unwrap();
        assert!(matches!(function.as_ref(), Function::Temporal(_)), "{name}");
        assert!(function.as_temporal().is_some(), "{name}");
        assert!(!function.is_lazy_temporal(), "{name}");
    }
    let window = registry.get_function("drasi.slidingWindow").unwrap();
    assert!(matches!(window.as_ref(), Function::LazyTemporal(_)));
    assert!(window.is_lazy_temporal());
    assert!(window.as_temporal().is_some());
}

#[test]
fn temporal_functions_validate_arguments_through_settle() {
    let registry = registry();
    for name in TEMPORAL.iter().copied().chain(["drasi.slidingWindow"]) {
        let function = registry.get_function(name).unwrap();
        let (captured, mut input) = capture_named(name, Vec::new(), 0, 0);
        let error = function
            .as_temporal()
            .unwrap()
            .settle(&captured, &mut input, None, true)
            .unwrap_err();
        let EvaluationError::FunctionError(error) = error else {
            panic!("{name} expected a function error");
        };
        assert_eq!(error.function_name, name);
        assert_eq!(error.error, FunctionEvaluationError::InvalidArgumentCount);
    }
}

#[tokio::test]
async fn awaiting_remains_scalar_and_callable_without_retained_runtime() {
    let registry = registry();
    let function = registry.get_function("drasi.awaiting").unwrap();
    assert!(function.as_temporal().is_none());
    let Function::Scalar(function) = function.as_ref() else {
        panic!("drasi.awaiting must remain scalar");
    };
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(1000, 2000)));
    let expression = ast::FunctionExpression {
        name: "drasi.awaiting".into(),
        args: Vec::new(),
        position_in_query: 10,
    };
    assert_eq!(
        function
            .call(&context, &expression, Vec::new())
            .await
            .unwrap(),
        VariableValue::Awaiting
    );
}

fn settle(
    captured: &CapturedCall,
    input: &mut RetainedInput,
) -> Result<SettledFunction, EvaluationError> {
    TrueNowOrLater.settle(captured, input, None, true)
}

#[test]
fn test_true_now_or_later_invalid_args_count() {
    for arguments in [
        vec![],
        vec![VariableValue::Bool(false)],
        vec![
            VariableValue::Bool(false),
            VariableValue::from(100),
            VariableValue::Null,
        ],
    ] {
        let (captured, mut input) = capture(arguments, 0, 0);
        assert_function_error(
            settle(&captured, &mut input),
            FunctionEvaluationError::InvalidArgumentCount,
        );
    }
}

#[test]
fn test_true_now_or_later_condition_true() {
    let (captured, mut input) = capture(
        vec![VariableValue::Bool(true), VariableValue::from(100)],
        0,
        0,
    );
    let result = settle(&captured, &mut input).unwrap();
    assert_eq!(result.value, VariableValue::Bool(true));
    assert!(result.tickets.is_empty());
}

#[test]
fn test_true_now_or_later_due_time_passed() {
    let (captured, mut input) = capture(
        vec![VariableValue::Bool(false), VariableValue::from(900)],
        800,
        1000,
    );
    let result = settle(&captured, &mut input).unwrap();
    assert_eq!(result.value, VariableValue::Bool(false));
    assert!(result.tickets.is_empty());
}

#[test]
fn test_true_now_or_later_schedule_future() {
    let (captured, mut input) = capture(
        vec![VariableValue::Bool(false), VariableValue::from(1500)],
        1000,
        1200,
    );
    let result = settle(&captured, &mut input).unwrap();
    assert_eq!(result.value, VariableValue::Awaiting);
    assert_requested_ticket(&result, &captured, 1000, 1500);
    assert!(input.tickets.is_empty());
    assert_eq!(input.next_ticket_generation, Generation(0));
}

#[test]
fn test_true_now_or_later_with_date() {
    let date = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
    let expected_timestamp = u64::try_from(
        date.and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
    )
    .unwrap();
    let (captured, mut input) = capture(
        vec![VariableValue::Bool(false), VariableValue::Date(date)],
        1000,
        2000,
    );
    let result = settle(&captured, &mut input).unwrap();
    assert_eq!(result.value, VariableValue::Awaiting);
    assert_requested_ticket(&result, &captured, 1000, expected_timestamp);
}

#[test]
fn test_true_now_or_later_invalid_condition_type() {
    let (captured, mut input) = capture(
        vec![VariableValue::from("not a bool"), VariableValue::from(100)],
        0,
        0,
    );
    assert_function_error(
        settle(&captured, &mut input),
        FunctionEvaluationError::InvalidArgument(0),
    );
}

#[test]
fn test_true_now_or_later_null_arguments() {
    for arguments in [
        vec![VariableValue::Null, VariableValue::from(100)],
        vec![VariableValue::Bool(false), VariableValue::Null],
    ] {
        let (captured, mut input) = capture(arguments, 0, 0);
        let result = settle(&captured, &mut input).unwrap();
        assert_eq!(result.value, VariableValue::Null);
        assert!(result.tickets.is_empty());
    }
}

#[test]
fn retained_input_can_schedule_without_an_anchor() {
    for condition in [false, true] {
        let (mut captured, mut input) = capture(
            vec![VariableValue::Bool(condition), VariableValue::from(100)],
            0,
            0,
        );
        captured.context.anchor = None;
        input.context.anchor = None;
        let result = settle(&captured, &mut input).unwrap();
        if condition {
            assert_eq!(result.value, VariableValue::Bool(true));
            assert!(result.tickets.is_empty());
        } else {
            assert_eq!(result.value, VariableValue::Awaiting);
            assert_eq!(result.tickets.len(), 1);
            assert_eq!(result.tickets[0].call, captured.call);
            assert_eq!(result.tickets[0].attribution, None);
        }
    }
}

fn capture(
    arguments: Vec<VariableValue>,
    transaction_time: u64,
    realtime: u64,
) -> (CapturedCall, RetainedInput) {
    capture_named("function", arguments, transaction_time, realtime)
}

fn capture_named(
    name: &str,
    arguments: Vec<VariableValue>,
    transaction_time: u64,
    realtime: u64,
) -> (CapturedCall, RetainedInput) {
    let mut input = fixtures::input();
    let clock = ClockStamp {
        transaction_time,
        realtime,
    };
    input.source_clock = clock;
    input.evaluated_at = clock;
    input.context.clock = clock;
    input.context.input_grouping_hash = 123;
    input.context.anchor = Some(Arc::new(Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::from([Arc::from("TestLabel")]),
            effective_from: 2000,
        },
        properties: Default::default(),
    }));
    let mut call = fixtures::call();
    call.site.position_in_query = 10;
    let captured = CapturedCall {
        call,
        expression: ast::FunctionExpression {
            name: name.into(),
            args: Vec::new(),
            position_in_query: 10,
        },
        arguments,
        key: ContributionKey::InputHash(input.context.input_grouping_hash),
        context: input.context.clone(),
    };
    (captured, input)
}

fn assert_function_error(
    result: Result<SettledFunction, EvaluationError>,
    expected: FunctionEvaluationError,
) {
    let Err(EvaluationError::FunctionError(error)) = result else {
        panic!("expected {expected:?}, got {result:?}");
    };
    assert_eq!(error.function_name, "function");
    assert_eq!(error.error, expected);
}

fn assert_requested_ticket(
    result: &SettledFunction,
    captured: &CapturedCall,
    original_time: u64,
    due_time: u64,
) {
    assert_eq!(result.tickets.len(), 1);
    let ticket = &result.tickets[0];
    assert_eq!(ticket.call, captured.call);
    assert_eq!(ticket.call.site.position_in_query, 10);
    assert_eq!(ticket.slot, 0);
    assert_eq!(ticket.cell, None);
    assert_eq!(ticket.activation, Generation(0));
    assert_eq!(ticket.original_time, original_time);
    assert_eq!(ticket.due_time, due_time);
    assert_eq!(
        ticket.attribution,
        Some(ElementReference::new("test_namespace", "test_id"))
    );
}
