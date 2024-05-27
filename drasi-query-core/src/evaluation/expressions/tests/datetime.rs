use chrono::{FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use std::sync::Arc;

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
            VariableValue::Integer((1448901144000 as i64).into())
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
            VariableValue::Integer((1448901144 as i64).into())
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
