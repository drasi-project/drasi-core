use chrono::{FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Local, LocalResult, DateTime, TimeZone};
use std::sync::Arc;
use std::str::FromStr;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

#[tokio::test]
async fn evaluate_local_date_time_yy_mm_dd() {
    let expr = "localdatetime('2020-11-04T19:32:24')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2020, 11, 4).unwrap();
        let naive_time = NaiveTime::from_hms_opt(19, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }

    let expr = "localdatetime('20201104T193224')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2020, 11, 4).unwrap();
        let naive_time = NaiveTime::from_hms_opt(19, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_local_date_time_yy_mm_dd_with_fraction() {
    let expr = "localdatetime('2020-11-04T19:32:24.1234')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2020, 11, 4).unwrap();
        let naive_time = NaiveTime::from_hms_micro_opt(19, 32, 24, 123400).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_local_date_time_yy_mm_dd_hour_only() {
    let expr = "localdatetime('2020-11-04T19')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2020, 11, 4).unwrap();
        let naive_time = NaiveTime::from_hms_opt(19, 0, 0).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_local_date_time_yy_ww_dd() {
    let expr = "localdatetime('2015-W30-2T214032')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 7, 21).unwrap();
        let naive_time = NaiveTime::from_hms_opt(21, 40, 32).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_local_date_time_yy_ddd() {
    let expr = "localdatetime('2015-202T21:40:32')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 7, 21).unwrap();
        let naive_time = NaiveTime::from_hms_opt(21, 40, 32).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_date_time_yy_mm_dd_utc() {
    let expr = "datetime('2015-W30-2T19:32:24Z')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 7, 21).unwrap();
        let naive_time = NaiveTime::from_hms_opt(19, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, None);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_date_time_yy_mm_dd_offset() {
    let expr = "datetime('2015-11-30T19:32:24+03:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
        let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, None);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }

    let expr = "datetime('2015-11-30T19:32:24-03:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
        let naive_time = NaiveTime::from_hms_opt(22, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, None);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_date_time_yy_ww_dd_offset() {
    let expr = "datetime('2015-W30-2T19:32:24+03:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 7, 21).unwrap();
        let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, None);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_date_time_yy_ww_dd_utc() {
    let expr = "datetime('2020-11-04T19:32:24Z')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2020, 11, 4).unwrap();
        let naive_time = NaiveTime::from_hms_opt(19, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, None);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_date_time_yy_mm_dd_timezone() {
    let expr = "datetime('2020-11-04T19:32:24[America/New_York]')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2020, 11, 5).unwrap();
        let naive_time = NaiveTime::from_hms_opt(0, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, Some("[America/New_York]".to_string()));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_date_time_yy_ww_dd_timezone() {
    let expr = "datetime('2015-W30-2T19:32:24[Europe/London]')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 7, 21).unwrap();
        let naive_time = NaiveTime::from_hms_opt(18, 32, 24).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, Some("[Europe/London]".to_string()));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn evaluate_local_date_time_creation_with_quarter() {
    let expr = "localdatetime({
        year: 1984, quarter: 3, dayOfQuarter: 45,
        hour: 12, minute: 31, second: 14, nanosecond: 645876123
    })";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(1984, 8, 14).unwrap();
        let naive_time = NaiveTime::from_hms_nano_opt(12, 31, 14, 645876123).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}


#[tokio::test]
async fn evaluate_zoned_date_time_yy_ww_dd_timezone_with_frac() {
    let expr = "datetime('2015-W30-2T19:32:24.1234[Europe/London]')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(2015, 7, 21).unwrap();
        let naive_time = NaiveTime::from_hms_micro_opt(18, 32, 24, 123400).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        let zoned_date_time = ZonedDateTime::new(date_time, Some("[Europe/London]".to_string()));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(zoned_date_time)
        );
    }
}

#[tokio::test]
async fn test_local_date_time_year() {
    let expr = "$param1.year";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    variables.insert(
        "param1".to_string().into(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(2020.into())
        );
    }
}

#[tokio::test]
async fn test_local_date_time_month() {
    let expr = "$param1.month";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    variables.insert(
        "param1".to_string().into(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(11.into())
        );
    }
}

#[tokio::test]
async fn test_local_date_time_week() {
    let expr = "$param1.week";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    variables.insert(
        "param1".to_string().into(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(48.into())
        );
    }
}

#[tokio::test]
async fn test_local_date_time_second() {
    let expr = "$param1.second";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    variables.insert(
        "param1".to_string().into(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_opt(9, 51, 12)
                .unwrap(),
        ),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(12.into())
        );
    }
}

#[tokio::test]
async fn test_local_date_time_microsecond() {
    let expr = "$param1.microsecond";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    variables.insert(
        "param1".to_string().into(),
        VariableValue::LocalDateTime(
            NaiveDate::from_ymd_opt(2020, 11, 23)
                .unwrap()
                .and_hms_micro_opt(9, 51, 12, 645876)
                .unwrap(),
        ),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(645876.into())
        );
    }
}

#[tokio::test]
async fn test_datetime_quarter() {
    let expr = "$param1.quarter";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    variables.insert(
        "param1".to_string().into(),
        VariableValue::ZonedDateTime(zoned_date_time),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(4.into())
        );
    }
}

#[tokio::test]
async fn test_datetime_epoch_millis() {
    let expr = "$param1.epochMillis";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    variables.insert(
        "param1".to_string().into(),
        VariableValue::ZonedDateTime(zoned_date_time),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(1448901144000_i64.into())
        );
    }
}

#[tokio::test]
async fn test_datetime_epoch_seconds() {
    let expr = "$param1.epochSeconds";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari);

    let mut variables = QueryVariables::new();

    let naive_date = NaiveDate::from_ymd_opt(2015, 11, 30).unwrap();
    let naive_time = NaiveTime::from_hms_opt(16, 32, 24).unwrap();
    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    let date_time =
        NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
            .unwrap();
    let zoned_date_time = ZonedDateTime::new(date_time, None);

    variables.insert(
        "param1".to_string().into(),
        VariableValue::ZonedDateTime(zoned_date_time),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(1448901144_i64.into())
        );
    }
}

#[tokio::test]
async fn test_local_datetime_creation_from_component() {
    let expr = "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, millisecond: 123, microsecond: 456, nanosecond: 789})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_date = NaiveDate::from_ymd_opt(1984, 10, 11).unwrap();
        let naive_time = NaiveTime::from_hms_nano_opt(12, 31, 14, 123456789).unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date.and_time(naive_time))
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_creation_from_component() {
    let expr = "datetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: '+01:00'})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_datetime = NaiveDate::from_ymd_opt(1984, 3, 7)
            .unwrap()
            .and_hms_micro_opt(12, 31, 14, 645876)
            .unwrap();
        let datetime = NaiveDateTime::and_local_timezone(
            &naive_datetime,
            FixedOffset::east_opt(3600).unwrap(),
        )
        .unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, Some("+01:00".to_string())))
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_creation_from_component_iana() {
    let expr = "datetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, second: 14, timezone: 'Europe/Stockholm'})";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_datetime = NaiveDate::from_ymd_opt(1984, 3, 7)
            .unwrap()
            .and_hms_micro_opt(12, 31, 14, 0)
            .unwrap();
        let datetime = NaiveDateTime::and_local_timezone(
            &naive_datetime,
            FixedOffset::east_opt(3600).unwrap(),
        )
        .unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(
                datetime,
                Some("Europe/Stockholm".to_string())
            ))
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_creation_epoch_seconds() {
    let expr = "datetime({epochSeconds: 1448901144, nanosecond:123})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_datetime = NaiveDate::from_ymd_opt(2015, 11, 30)
            .unwrap()
            .and_hms_nano_opt(16, 32, 24, 123)
            .unwrap();
        let datetime =
            NaiveDateTime::and_local_timezone(&naive_datetime, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, None))
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_creation_epoch_millis() {
    let expr = "datetime({epochMillis: 424797300000})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_datetime = NaiveDate::from_ymd_opt(1983, 6, 18)
            .unwrap()
            .and_hms_nano_opt(15, 15, 0, 0)
            .unwrap();
        let datetime =
            NaiveDateTime::and_local_timezone(&naive_datetime, FixedOffset::east_opt(0).unwrap())
                .unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, None))
        );
    }
}

#[tokio::test]
async fn test_local_datetime_duration_addition() {
    let expr = "localdatetime('2020-11-04T19:32:24') + duration('P37DT4H5M6S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_date = NaiveDate::from_ymd_opt(2020, 12, 11).unwrap();
        let naive_time = NaiveTime::from_hms_opt(23, 37, 30).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_duration_addition() {
    let expr = "datetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: '+01:00'}) + duration('P37DT4H5M6S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_datetime = NaiveDate::from_ymd_opt(1984, 4, 13)
            .unwrap()
            .and_hms_micro_opt(16, 36, 20, 645876)
            .unwrap();
        let datetime = NaiveDateTime::and_local_timezone(
            &naive_datetime,
            FixedOffset::east_opt(3600).unwrap(),
        )
        .unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, Some("+01:00".to_string())))
        );
    }
}

#[tokio::test]
async fn test_local_datetime_duration_subtraction() {
    let expr = "localdatetime('2020-11-04T19:32:24') - duration('P37DT4H5M6S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_date = NaiveDate::from_ymd_opt(2020, 9, 28).unwrap();
        let naive_time = NaiveTime::from_hms_opt(15, 27, 18).unwrap();
        let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalDateTime(naive_date_time)
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_duration_subtraction() {
    let expr = "datetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: '+01:00'}) - duration('P37DT4H5M6S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_datetime = NaiveDate::from_ymd_opt(1984, 1, 30)
            .unwrap()
            .and_hms_micro_opt(8, 26, 8, 645876)
            .unwrap();
        let datetime = NaiveDateTime::and_local_timezone(
            &naive_datetime,
            FixedOffset::east_opt(3600).unwrap(),
        )
        .unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, Some("+01:00".to_string())))
        );
    }
}

#[tokio::test]
async fn test_local_datetime_ne() {
    let expr = "localdatetime('2020-11-04T19:32:24') != localdatetime('2020-01-04T19:32:24')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_local_datetime_lt() {
    let expr = "localdatetime('2020-11-04T19:32:24') < localdatetime('2021-01-01T00:00:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_local_datetime_le() {
    let expr = "localdatetime('2020-11-04T19:32:24') <= localdatetime('2020-11-04T20:32:24')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_local_datetime_gt() {
    let expr = "localdatetime('2020-11-04T19:32:24') > localdatetime('2020-01-01T00:00:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_local_datetime_ge() {
    let expr = "localdatetime('2020-11-04T19:32:24') >= localdatetime('2020-11-04T19:32:24')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_lt() {
    let expr = "datetime('2020-11-04T19:32:24+00:00') < datetime('2020-11-04T20:32:24+00:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_le() {
    let expr = "datetime('2020-11-04T19:32:24+00:00') <= datetime('2020-11-04T20:32:24+00:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_gt() {
    let expr = "datetime('2020-11-04T19:32:24+00:00') > datetime('2020-01-01T00:00:00+00:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_zoned_datetime_ge() {
    let expr = "datetime('2020-11-04T19:32:24+00:00') >= datetime('2020-11-04T19:32:24+00:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn test_local_date_time_get_current() {
    let expr = "localdatetime()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let now = Local::now().naive_local();
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::LocalDateTime(result) => result,
        _ => panic!("Failed to get local date time"),
    };
    // ensure that the result is within 500ms of the current time
    assert!(now.signed_duration_since(result).num_milliseconds().abs() < 500);
}

#[tokio::test]
async fn test_local_date_time_with_timezone() {
    let expr = "localdatetime({timezone: 'Asia/Shanghai'})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let current_time_in_shanghai = Local::now().with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap()).naive_local();
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::LocalDateTime(result) => result,
        _ => panic!("Failed to get local date time"),
    };
    // ensure that the result is within 500ms of the current time
    assert!(current_time_in_shanghai.signed_duration_since(result).num_milliseconds().abs() < 500);

}


#[tokio::test]
async fn test_local_date_time_transaction() {
    let expr = "localdatetime.transaction()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::LocalDateTime(result) => result,
        _ => panic!("Failed to get local date time"),
    };

    assert_eq!(result, NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap());
}

#[tokio::test]
async fn test_local_date_time_statement() {
    let expr = "localdatetime.statement()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::LocalDateTime(result) => result,
        _ => panic!("Failed to get local date time"),
    };

    assert_eq!(result, NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap());
}

#[tokio::test]
async fn test_local_date_time_realtime() {
    let expr = "localdatetime.realtime()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::LocalDateTime(result) => result,
        _ => panic!("Failed to get local date time"),
    };

    assert_eq!(result,  NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap());
}


#[tokio::test]
async fn test_local_date_time_truncate() {
    let naive_datetime = NaiveDate::from_ymd_opt(2017,11,11).unwrap().and_hms_nano_opt(12, 31, 14, 645876123).unwrap(); 
    let local_datetime = VariableValue::LocalDateTime(naive_datetime);

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    variables.insert("param1".to_string().into(), local_datetime);

    let expr = "localdatetime.truncate('millennium', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator.evaluate_expression(&context, &expr).await.unwrap();
    assert_eq!(result, VariableValue::LocalDateTime(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()));

    let expr = "localdatetime.truncate('year', $param1, {day: 2})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator.evaluate_expression(&context, &expr).await.unwrap();
    assert_eq!(result, VariableValue::LocalDateTime(NaiveDate::from_ymd_opt(2017, 1, 2).unwrap().and_hms_opt(0, 0, 0).unwrap()));

    let expr = "localdatetime.truncate('month', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator.evaluate_expression(&context, &expr).await.unwrap();
    assert_eq!(result, VariableValue::LocalDateTime(NaiveDate::from_ymd_opt(2017, 11, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()));

    let expr = "localdatetime.truncate('day', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator.evaluate_expression(&context, &expr).await.unwrap();
    assert_eq!(result, VariableValue::LocalDateTime(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap().and_hms_opt(0, 0, 0).unwrap()));

    let expr = "localdatetime.truncate('hour', $param1, {nanosecond: 2})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator.evaluate_expression(&context, &expr).await.unwrap();
    assert_eq!(result, VariableValue::LocalDateTime(NaiveDate::from_ymd_opt(2017, 11, 11).unwrap().and_hms_nano_opt(12, 0, 0,2).unwrap()));
}


#[tokio::test]
async fn test_zoned_datetime_get_current() {
    let expr = "datetime()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let now = Local::now().with_timezone(&FixedOffset::east_opt(0).unwrap());

    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::ZonedDateTime(result) => result,
        _ => panic!("Failed to get zoned date time"),
    };
    // ensure that the result is within 500ms of the current time
    assert!(now.signed_duration_since(result.datetime()).num_milliseconds().abs() < 500);
}


#[tokio::test]
async fn test_zoned_datetime_get_current_time_in_shanghai() {
    let expr = "datetime({timezone: 'Asia/Shanghai'})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let now = Local::now().with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap());

    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::ZonedDateTime(result) => result,
        _ => panic!("Failed to get zoned date time"),
    };
    assert!(now.signed_duration_since(result.datetime()).num_milliseconds().abs() < 500);
}

#[tokio::test]
async fn test_zoned_datetime_transaction() {
    let expr = "datetime.transaction()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::ZonedDateTime(result) => result,
        _ => panic!("Failed to get zoned date time"),
    };

    let naive_date_time = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);
}

#[tokio::test]
async fn test_zoned_datetime_statement() {
    let expr = "datetime.statement()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::ZonedDateTime(result) => result,
        _ => panic!("Failed to get zoned date time"),
    };

    let naive_date_time = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);
}


#[tokio::test]
async fn test_zoned_datetime_realtime() {
    let expr = "datetime.realtime()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await.unwrap() {
        VariableValue::ZonedDateTime(result) => result,
        _ => panic!("Failed to get zoned date time"),
    };

    let naive_date_time = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(0).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);
}

#[tokio::test]
async fn test_zoned_datetime_truncate() {
    let naive_datetime = NaiveDate::from_ymd_opt(2017,11,11).unwrap().and_hms_nano_opt(12, 31, 14, 645876123).unwrap();
    let fixed_offset = FixedOffset::from_str("+03:00").unwrap();
    let datetime = match fixed_offset.from_local_datetime(&naive_datetime) {
        LocalResult::Single(zoned_datetime) => zoned_datetime.fixed_offset(),
        _ => panic!("Failed to create zoned date time"),
    };
    let zoned_datetime = VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, Some("+03:00".to_string())));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    variables.insert("param1".to_string().into(), zoned_datetime);

    let expr = "datetime.truncate('millennium', $param1, {timezone: 'Europe/Stockholm'})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedDateTime(result)) => result,
        _ => panic!("Failed to get zoned datetime"),
    };
    let naive_date_time = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);
    assert_eq!(*result.timezone(), Some("Europe/Stockholm".to_string()));


    let expr = "datetime.truncate('year', $param1, {day: 5})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedDateTime(result)) => result,
        _ => panic!("Failed to get zoned datetime"),
    };
    let naive_date_time = NaiveDate::from_ymd_opt(2017,1,5).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600 * 3).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);

    let expr = "datetime.truncate('month', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedDateTime(result)) => result,
        _ => panic!("Failed to get zoned datetime"),
    };
    let naive_date_time = NaiveDate::from_ymd_opt(2017,11,1).unwrap().and_hms_opt(0, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600 * 3).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);

    let expr = "datetime.truncate('day', $param1, {millisecond: 2})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedDateTime(result)) => result,
        _ => panic!("Failed to get zoned datetime"),
    };
    let naive_date_time = NaiveDate::from_ymd_opt(2017,11,11).unwrap().and_hms_milli_opt(0, 0, 0,2).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600 * 3).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);

    let expr = "datetime.truncate('hour', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedDateTime(result)) => result,
        _ => panic!("Failed to get zoned datetime"),
    };
    let naive_date_time = NaiveDate::from_ymd_opt(2017,11,11).unwrap().and_hms_opt(12, 0, 0).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600 * 3).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);


    let expr = "datetime.truncate('second', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context = ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedDateTime(result)) => result,
        _ => panic!("Failed to get zoned datetime"),
    };
    let naive_date_time = NaiveDate::from_ymd_opt(2017,11,11).unwrap().and_hms_opt(12, 31, 14).unwrap();
    let date_time =
            NaiveDateTime::and_local_timezone(&naive_date_time, FixedOffset::east_opt(3600 * 3).unwrap())
                .unwrap();
    assert_eq!(*result.datetime(), date_time);
}