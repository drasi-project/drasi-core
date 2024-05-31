use std::sync::Arc;

use super::temporal_instant;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::{context::QueryVariables, InstantQueryClock};
use chrono::{FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use drasi_query_ast::ast;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_truncate_date_time_year() {
    let truncate_datetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    let args = vec![
        VariableValue::String("year".to_string()),
        VariableValue::ZonedDateTime(zoned_date_time),
    ];
    let result = truncate_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let expected_naive_date_time = NaiveDate::from_ymd_opt(2015, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let expected_date_time = NaiveDateTime::and_local_timezone(
        &expected_naive_date_time,
        FixedOffset::east_opt(0).unwrap(),
    )
    .unwrap();
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(expected_date_time, None))
    );
}

#[tokio::test]
async fn test_truncate_date_time_day() {
    let truncate_datetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    let args = vec![
        VariableValue::String("day".to_string()),
        VariableValue::ZonedDateTime(zoned_date_time),
    ];
    let result = truncate_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let expected_naive_date_time = NaiveDate::from_ymd_opt(2015, 11, 30)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let expected_date_time = NaiveDateTime::and_local_timezone(
        &expected_naive_date_time,
        FixedOffset::east_opt(3600).unwrap(),
    )
    .unwrap();
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(expected_date_time, None))
    );
}

#[tokio::test]
async fn test_truncate_date_time_minute() {
    let truncate_datetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::west_opt(10800).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::ZonedDateTime(zoned_date_time),
    ];
    let result = truncate_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let expected_naive_date_time = NaiveDate::from_ymd_opt(2015, 11, 30)
        .unwrap()
        .and_hms_opt(16, 32, 0)
        .unwrap();
    let expected_date_time = NaiveDateTime::and_local_timezone(
        &expected_naive_date_time,
        FixedOffset::west_opt(10800).unwrap(),
    )
    .unwrap();
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(expected_date_time, None))
    );
}

#[tokio::test]
async fn test_truncate_date_time_millisecond() {
    let truncate_datetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_micro_opt(16, 32, 24, 123456).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::west_opt(10800).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    let args = vec![
        VariableValue::String("millisecond".to_string()),
        VariableValue::ZonedDateTime(zoned_date_time),
    ];
    let result = truncate_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let expected_naive_date_time = NaiveDate::from_ymd_opt(2015, 11, 30)
        .unwrap()
        .and_hms_micro_opt(16, 32, 24, 123000)
        .unwrap();
    let expected_date_time = NaiveDateTime::and_local_timezone(
        &expected_naive_date_time,
        FixedOffset::west_opt(10800).unwrap(),
    )
    .unwrap();
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(expected_date_time, None))
    );
}

#[tokio::test]
async fn test_truncate_date_time_minute_with_timezone() {
    let truncate_datetime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::west_opt(10800).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, Some("[Europe/Stockholm]".to_string()));

    let args = vec![
        VariableValue::String("minute".to_string()),
        VariableValue::ZonedDateTime(zoned_date_time),
    ];
    let result = truncate_datetime
        .call(&context, &get_func_expr(), args.clone())
        .await;

    let expected_naive_date_time = NaiveDate::from_ymd_opt(2015, 11, 30)
        .unwrap()
        .and_hms_opt(16, 32, 0)
        .unwrap();
    let expected_date_time = NaiveDateTime::and_local_timezone(
        &expected_naive_date_time,
        FixedOffset::west_opt(10800).unwrap(),
    )
    .unwrap();
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(
            expected_date_time,
            Some("[Europe/Stockholm]".to_string())
        ))
    );
}
