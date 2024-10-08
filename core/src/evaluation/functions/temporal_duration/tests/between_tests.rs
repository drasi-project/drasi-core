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

use crate::evaluation::functions::temporal_duration::Between;
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
async fn test_between_date() {
    let between_date = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap());
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 7).unwrap());
    let args = vec![date1, date2];
    let result = between_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(3), 0, 0))
    );

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap());
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2019, 3, 14).unwrap());
    let args = vec![date1, date2];
    let result = between_date
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(ChronoDuration::days(-601), 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_time() {
    let between_local_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_opt(21, 32, 24).unwrap());
    let time2 = VariableValue::LocalTime(NaiveTime::from_hms_opt(16, 11, 23).unwrap());
    let args = vec![time1, time2];
    let result = between_local_time
        .call(&context, &get_func_expr(), args.clone())
        .await;
    let chrono_duration =
        ChronoDuration::hours(-5) + ChronoDuration::minutes(-21) + ChronoDuration::seconds(-1);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_date_local_time() {
    let between_date_local_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap());
    let time = VariableValue::LocalTime(NaiveTime::from_hms_opt(16, 11, 23).unwrap());
    let args = vec![date, time];
    let result = between_date_local_time
        .call(&context, &get_func_expr(), args.clone())
        .await;
    let chrono_duration =
        ChronoDuration::hours(16) + ChronoDuration::minutes(11) + ChronoDuration::seconds(23);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_date_time() {
    let between_local_date_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date_time1 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2020, 11, 4).unwrap(),
        NaiveTime::from_hms_opt(21, 32, 24).unwrap(),
    ));
    let date_time2 = VariableValue::LocalDateTime(NaiveDateTime::new(
        NaiveDate::from_ymd_opt(2020, 11, 7).unwrap(),
        NaiveTime::from_hms_opt(16, 11, 23).unwrap(),
    ));
    let args = vec![date_time1, date_time2];
    let result = between_local_date_time
        .call(&context, &get_func_expr(), args.clone())
        .await;
    let chrono_duration = ChronoDuration::days(3)
        + ChronoDuration::hours(-5)
        + ChronoDuration::minutes(-21)
        + ChronoDuration::seconds(-1);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_date_local_date_time() {
    let between_local_date_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(1984, 10, 11).unwrap());
    let date_time2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_milli_opt(21, 40, 32, 142)
            .unwrap(),
    );
    let args = vec![date1, date_time2];
    let result = between_local_date_time
        .call(&context, &get_func_expr(), args.clone())
        .await;
    let chrono_duration = ChronoDuration::days(1)
        + ChronoDuration::hours(21)
        + ChronoDuration::minutes(40)
        + ChronoDuration::seconds(32)
        + ChronoDuration::milliseconds(142);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_date_zoned_date_time() {
    let between_zoned_date_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(1984, 10, 11).unwrap());
    let naive_datetime2 = NaiveDate::from_ymd_opt(1984, 10, 12)
        .unwrap()
        .and_hms_opt(11, 23, 34)
        .unwrap()
        .and_local_timezone(FixedOffset::east_opt(0).unwrap())
        .unwrap();
    let date_time2 = VariableValue::ZonedDateTime(ZonedDateTime::new(naive_datetime2, None));
    let args = vec![date1, date_time2];
    let result = between_zoned_date_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration = ChronoDuration::days(1)
        + ChronoDuration::hours(11)
        + ChronoDuration::minutes(23)
        + ChronoDuration::seconds(34);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_date_zoned_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(1984, 10, 11).unwrap());
    let naive_time2 = NaiveTime::from_hms_opt(11, 23, 34).unwrap();
    let date_time2 = VariableValue::ZonedTime(ZonedTime::new(
        naive_time2,
        FixedOffset::east_opt(0).unwrap(),
    ));
    let args = vec![date1, date_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(11) + ChronoDuration::minutes(23) + ChronoDuration::seconds(34);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_time_and_local_datetime() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_opt(7, 11, 24).unwrap());
    let local_datetime2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap(),
    );

    let args = vec![time1, local_datetime2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(4) + ChronoDuration::minutes(12) + ChronoDuration::seconds(10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_time_and_zoned_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_opt(7, 11, 24).unwrap());
    let zoned_time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(11, 43, 34).unwrap(),
        FixedOffset::east_opt(0).unwrap(),
    ));

    let args = vec![time1, zoned_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(4) + ChronoDuration::minutes(32) + ChronoDuration::seconds(10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_time_and_zoned_datetime() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let time1 = VariableValue::LocalTime(NaiveTime::from_hms_opt(7, 11, 24).unwrap());
    let zoned_datetime2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(0).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![time1, zoned_datetime2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(4) + ChronoDuration::minutes(12) + ChronoDuration::seconds(10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_time1 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(7, 11, 24).unwrap(),
        FixedOffset::west_opt(1).unwrap(),
    ));
    let zoned_time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(11, 43, 34).unwrap(),
        FixedOffset::east_opt(2).unwrap(),
    ));

    let args = vec![zoned_time1, zoned_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(4) + ChronoDuration::minutes(32) + ChronoDuration::seconds(7);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_time_local_date_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_time1 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(7, 11, 24).unwrap(),
        FixedOffset::west_opt(1).unwrap(),
    ));
    let local_date_time2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap(),
    );

    let args = vec![zoned_time1, local_date_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(4) + ChronoDuration::minutes(12) + ChronoDuration::seconds(10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_time_zoned_date_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_time1 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(7, 11, 24).unwrap(),
        FixedOffset::west_opt(3600).unwrap(),
    ));
    let zoned_date_time2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(7200).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![zoned_time1, zoned_date_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(1) + ChronoDuration::minutes(12) + ChronoDuration::seconds(10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_betweeen_local_date_time_date() {
    let between_local_date_time_date = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let local_date_time1 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 10)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap(),
    );
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(1984, 10, 12).unwrap());

    let args = vec![local_date_time1, date2];
    let result = between_local_date_time_date
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration = ChronoDuration::days(1)
        + ChronoDuration::hours(12)
        + ChronoDuration::minutes(36)
        + ChronoDuration::seconds(26);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_date_time_zoned_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let local_date_time1 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap(),
    );
    let zoned_time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(7, 11, 24).unwrap(),
        FixedOffset::west_opt(1).unwrap(),
    ));

    let args = vec![local_date_time1, zoned_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(-4) + ChronoDuration::minutes(-12) + ChronoDuration::seconds(-10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_local_date_time_zoned_date_time() {
    let between_zoned_time = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let local_date_time1 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap(),
    );
    let zoned_date_time2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 13)
            .unwrap()
            .and_hms_opt(18, 31, 53)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![local_date_time1, zoned_date_time2];
    let result = between_zoned_time
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration = ChronoDuration::days(1)
        + ChronoDuration::hours(7)
        + ChronoDuration::minutes(8)
        + ChronoDuration::seconds(19);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_datetime() {
    let between_zoned_datetime = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_datetime1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let zoned_datetime2 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 21)
            .unwrap()
            .and_hms_opt(18, 31, 53)
            .unwrap()
            .and_local_timezone(FixedOffset::east_opt(7200).unwrap())
            .unwrap(),
        None,
    ));

    let args = vec![zoned_datetime1, zoned_datetime2];
    let result = between_zoned_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration = ChronoDuration::days(9)
        + ChronoDuration::hours(4)
        + ChronoDuration::minutes(8)
        + ChronoDuration::seconds(19);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_datetime_date() {
    let between_zoned_datetime = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_datetime1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(1984, 10, 13).unwrap());

    let args = vec![zoned_datetime1, date2];
    let result = between_zoned_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(12) + ChronoDuration::minutes(36) + ChronoDuration::seconds(26);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_datetime_local_time() {
    let between_zoned_datetime = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_datetime1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let local_time2 = VariableValue::LocalTime(NaiveTime::from_hms_opt(7, 11, 24).unwrap());

    let args = vec![zoned_datetime1, local_time2];
    let result = between_zoned_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(-4) + ChronoDuration::minutes(-12) + ChronoDuration::seconds(-10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_datetime_zoned_time() {
    let between_zoned_datetime = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_datetime1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let zoned_time2 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(7, 11, 24).unwrap(),
        FixedOffset::east_opt(3600).unwrap(),
    ));

    let args = vec![zoned_datetime1, zoned_time2];
    let result = between_zoned_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration =
        ChronoDuration::hours(6) + ChronoDuration::minutes(12) + ChronoDuration::seconds(10);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}

#[tokio::test]
async fn test_between_zoned_datetime_local_date_time() {
    let between_zoned_datetime = Between {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let zoned_datetime1 = VariableValue::ZonedDateTime(ZonedDateTime::new(
        NaiveDate::from_ymd_opt(1984, 10, 12)
            .unwrap()
            .and_hms_opt(11, 23, 34)
            .unwrap()
            .and_local_timezone(FixedOffset::west_opt(3600).unwrap())
            .unwrap(),
        None,
    ));
    let local_date_time2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(1984, 10, 13)
            .unwrap()
            .and_hms_opt(18, 31, 53)
            .unwrap(),
    );

    let args = vec![zoned_datetime1, local_date_time2];
    let result = between_zoned_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let chrono_duration = ChronoDuration::days(1)
        + ChronoDuration::hours(7)
        + ChronoDuration::minutes(8)
        + ChronoDuration::seconds(19);
    assert_eq!(
        result.unwrap(),
        VariableValue::Duration(Duration::new(chrono_duration, 0, 0))
    );
}
