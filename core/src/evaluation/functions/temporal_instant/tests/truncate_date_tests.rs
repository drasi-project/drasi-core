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
use chrono::NaiveDate;
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
async fn test_truncate_month() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("month".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 1).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_year() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("year".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_decade() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("decade".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 4).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2010, 1, 1).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_day() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("day".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_century() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("century".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_weekyear() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("weekYear".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 1, 2).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_quarter() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("quarter".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;

    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 10, 1).unwrap())
    );

    let args = vec![
        VariableValue::String("quarter".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 2, 11).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;

    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 1, 1).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_week() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("week".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap()),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 6).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_month_with_map_component() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("month".to_string(), VariableValue::from(2));
    let args = vec![
        VariableValue::String("month".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 12, 1).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_year_with_map_component() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("year".to_string(), VariableValue::from(4));
    let args = vec![
        VariableValue::String("year".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2018, 11, 4).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2021, 1, 1).unwrap())
    );

    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("year".to_string(), VariableValue::from(4));
    map.insert("month".to_string(), VariableValue::from(2));
    map.insert("day".to_string(), VariableValue::from(2));
    let args = vec![
        VariableValue::String("year".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2018, 11, 4).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2021, 2, 2).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_day_with_map_component() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("day".to_string(), VariableValue::from(4));
    let args = vec![
        VariableValue::String("day".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2018, 11, 4).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2018, 11, 7).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_week_with_map_component() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("dayOfWeek".to_string(), VariableValue::from(2));
    let args = vec![
        VariableValue::String("week".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 7).unwrap())
    );
}

#[tokio::test]
async fn test_truncate_weekyear_with_map_component() {
    let truncate_date = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("dayOfWeek".to_string(), VariableValue::from(6));
    let args = vec![
        VariableValue::String("weekYear".to_string()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap()),
        VariableValue::Object(map),
    ];
    let result = truncate_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017, 1, 7).unwrap())
    );
}
