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

use super::temporal_instant;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};
use chrono::NaiveTime;
use drasi_query_ast::ast;
use std::collections::BTreeMap;
use std::sync::Arc;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_truncate_hour() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hour".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 0, 0).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_minute() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 0).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_day() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("day".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_millisecond() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("millisecond".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(9, 51, 12, 645876123).unwrap()),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(9, 51, 12, 645000000).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_microsecond() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("microsecond".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(9, 51, 12, 645876123).unwrap()),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(9, 51, 12, 645876000).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_hour_with_map() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let mut map = BTreeMap::new();
    map.insert("hour".to_string(), VariableValue::from(2));
    map.insert("second".to_string(), VariableValue::from(23));
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hour".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(11, 0, 23).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_minute_with_map() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let mut map = BTreeMap::new();
    map.insert("second".to_string(), VariableValue::from(41));
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 41).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_second_with_map() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let mut map = BTreeMap::new();
    map.insert("second".to_string(), VariableValue::from(41));
    map.insert("millisecond".to_string(), VariableValue::from(123));
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(9, 51, 41, 123).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_second_with_map_microsecond() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let mut map = BTreeMap::new();
    map.insert("microsecond".to_string(), VariableValue::from(123));
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("second".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_micro_opt(9, 51, 12, 123).unwrap())
    );
}
