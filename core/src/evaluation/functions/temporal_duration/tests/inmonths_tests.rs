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

use crate::evaluation::functions::temporal_duration::InMonths;
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
async fn test_date_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
        VariableValue::Date(NaiveDate::from_ymd_opt(2022, 2, 4).unwrap()),
    ];

    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 1, 3))
    );

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 3, 4).unwrap());
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2021, 9, 14).unwrap());
    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 1, 6))
    );
}

#[tokio::test]
async fn test_date_local_date_time_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 3, 4).unwrap());
    let date2 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2021, 9, 14).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 1, 6))
    );
}

#[tokio::test]
async fn test_date_zoned_date_time_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 3, 4).unwrap());
    let date2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2021, 1, 14)
            .unwrap()
            .and_hms_opt(16, 32, 24)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 10))
    );
}

#[tokio::test]
async fn test_local_time_zoned_time_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_opt(16, 32, 24).unwrap());
    let time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
        FixedOffset::east_opt(3600).unwrap(),
    ));

    let args = vec![time1, time2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0))
    );
}

#[tokio::test]
async fn test_local_datetime_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));
    let date2 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2019, 8, 14).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, -7))
    );

    let date1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2019, 8, 14).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));
    let date2 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2019, 5, 14).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, -3))
    );

    let date1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2019, 8, 14).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));
    let date2 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2012, 1, 14).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), -7, -7))
    );
}

#[tokio::test]
async fn test_local_datetime_date_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2018, 8, 14).unwrap());

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), -1, -7))
    );
}

#[tokio::test]
async fn test_local_datetime_zoned_date_time_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4).unwrap(),
        NaiveTime::from_hms_opt(16, 32, 24).unwrap(),
    ));
    let date2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2018, 8, 14)
            .unwrap()
            .and_hms_opt(16, 32, 24)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), -1, -7))
    );
}

#[tokio::test]
async fn test_zoned_datetime_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4)
            .unwrap()
            .and_hms_opt(16, 32, 24)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let date2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2018, 11, 14)
            .unwrap()
            .and_hms_opt(16, 32, 24)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), -1, -4))
    );
}

#[tokio::test]
async fn test_zoned_datetime_local_time_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4)
            .unwrap()
            .and_hms_opt(16, 32, 24)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let date2 = VariableValue::LocalTime(NaiveTime::from_hms_opt(16, 32, 24).unwrap());

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0))
    );
}

#[tokio::test]
async fn test_zoned_datetime_date_inmonths() {
    let in_months = InMonths {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(2020, 3, 4)
            .unwrap()
            .and_hms_opt(16, 32, 24)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2018, 11, 14).unwrap());

    let args = vec![date1, date2];
    let result = in_months
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(0), -1, -4))
    );
}
