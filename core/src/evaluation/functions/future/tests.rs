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

use drasi_query_ast::ast;

use crate::evaluation::functions::future::true_now_or_later::TrueNowOrLater;
use crate::evaluation::functions::TemporalScalar;
use crate::evaluation::temporal::fixtures;
use crate::evaluation::temporal::runtime::frame::CapturedCall;
use crate::evaluation::temporal::ContributionKey;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::FunctionEvaluationError;

#[test]
fn true_now_or_later_settles_without_a_queue() {
    let captured = CapturedCall {
        call: fixtures::call(),
        expression: ast::FunctionExpression {
            name: "function".into(),
            args: Vec::new(),
            position_in_query: fixtures::call().site.position_in_query,
        },
        arguments: vec![VariableValue::Bool(true), VariableValue::from(100)],
        key: ContributionKey::InputHash(0),
        context: fixtures::input().context,
    };
    let mut input = fixtures::input();
    let result = TrueNowOrLater
        .settle(&captured, &mut input, None, true)
        .unwrap();
    assert_eq!(result.value, VariableValue::Bool(true));
    assert!(result.tickets.is_empty());
}

#[test]
fn true_now_or_later_rejects_wrong_arity_without_a_queue() {
    let captured = CapturedCall {
        call: fixtures::call(),
        expression: ast::FunctionExpression {
            name: "function".into(),
            args: Vec::new(),
            position_in_query: fixtures::call().site.position_in_query,
        },
        arguments: Vec::new(),
        key: ContributionKey::InputHash(0),
        context: fixtures::input().context,
    };
    let mut input = fixtures::input();
    let error = TrueNowOrLater
        .settle(&captured, &mut input, None, true)
        .unwrap_err();
    let crate::evaluation::EvaluationError::FunctionError(error) = error else {
        panic!("expected function error");
    };
    assert_eq!(error.error, FunctionEvaluationError::InvalidArgumentCount);
}
