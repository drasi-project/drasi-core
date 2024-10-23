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
async fn test_truncate_local_date_time_year() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("year".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_month() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("month".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_week() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("week".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 6)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_weekyear() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("weekYear".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 1, 2)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_quarter() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("quarter".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 10, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_minute() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_year_with_map() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("year".to_string(), VariableValue::from(4));
    map.insert("month".to_string(), VariableValue::from(7));
    let args = vec![
        VariableValue::String("year".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
        VariableValue::Object(map),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2023, 7, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_month_with_map() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("day".to_string(), VariableValue::from(4));
    map.insert("hour".to_string(), VariableValue::from(7));
    let args = vec![
        VariableValue::String("month".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
        VariableValue::Object(map),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 4)
                .unwrap()
                .and_hms_opt(7, 0, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_week_with_map() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("dayOfWeek".to_string(), VariableValue::from(3));
    map.insert("second".to_string(), VariableValue::from(7));
    map.insert("microsecond".to_string(), VariableValue::from(126));
    let args = vec![
        VariableValue::String("week".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
        VariableValue::Object(map),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 8)
                .unwrap()
                .and_hms_micro_opt(0, 0, 7, 126)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_minute_with_map() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("minute".to_string(), VariableValue::from(3));
    map.insert("second".to_string(), VariableValue::from(7));
    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
        VariableValue::Object(map),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_micro_opt(9, 54, 7, 0)
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_truncate_local_date_time_hour_with_map() {
    let truncate_localdatetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert("millisecond".to_string(), VariableValue::from(14));
    let args = vec![
        VariableValue::String("hour".to_string()),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
        VariableValue::Object(map),
    ];
    let result = truncate_localdatetime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2017, 11, 11)
                .unwrap()
                .and_hms_milli_opt(9, 0, 0, 14)
                .unwrap()
        )
    );
}
