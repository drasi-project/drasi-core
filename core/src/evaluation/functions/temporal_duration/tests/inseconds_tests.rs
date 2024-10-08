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

use std::sync::Arc;

use crate::evaluation::functions::temporal_duration::InSeconds;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::duration::Duration;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::{context::QueryVariables, InstantQueryClock};
use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use drasi_query_ast::ast;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_date_inseconds() {
    let in_seconds = InSeconds {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap());
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 27).unwrap());

    let args = vec![date1, date2];
    let result = in_seconds
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::hours(552), 0, 0))
    );
}

#[tokio::test]
async fn test_local_time_inseconds() {
    let in_seconds = InSeconds {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(16, 32, 24, 123).unwrap());
    let time2 =
        VariableValue::LocalTime(NaiveTime::from_hms_micro_opt(16, 32, 24, 241234).unwrap());

    let args = vec![time1, time2];
    let result = in_seconds
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::microseconds(118234), 0, 0))
    );
}

#[tokio::test]
async fn test_local_time_zoned_time_inseconds() {
    let in_seconds = InSeconds {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(16, 32, 24, 123).unwrap());
    let time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_nano_opt(16, 32, 23, 24123412).unwrap(),
        FixedOffset::east_opt(3600).unwrap(),
    ));

    let args = vec![time1, time2];
    let result = in_seconds
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::seconds(-1) + ChronoDuration::nanoseconds(-98876588),
            0,
            0
        ))
    );
}

#[tokio::test]
async fn test_local_datetime_zoned_time_inseconds() {
    let in_seconds = InSeconds {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4).unwrap(),
        NaiveTime::from_hms_milli_opt(16, 1, 24, 123).unwrap(),
    ));
    let time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_nano_opt(16, 32, 23, 24123412).unwrap(),
        FixedOffset::east_opt(3600).unwrap(),
    ));

    let args = vec![time1, time2];
    let result = in_seconds
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::seconds(1858) + ChronoDuration::nanoseconds(901123412),
            0,
            0
        ))
    );
}

#[tokio::test]
async fn test_zoned_datetime_inseconds() {
    let in_seconds = InSeconds {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4)
            .unwrap()
            .and_hms_milli_opt(16, 32, 24, 123)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let time2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4)
            .unwrap()
            .and_hms_nano_opt(16, 32, 24, 24123412)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![time1, time2];
    let result = in_seconds
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::hours(1)
                + ChronoDuration::minutes(59)
                + ChronoDuration::seconds(59)
                + ChronoDuration::nanoseconds(901123412),
            0,
            0
        ))
    );
}
