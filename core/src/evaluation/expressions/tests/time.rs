use chrono::{FixedOffset, Local, NaiveTime};
use std::str::FromStr;
use std::sync::Arc;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

#[tokio::test]
async fn evalute_local_time_hh_mm_ss() {
    let expr = "localtime('12:54:03')";
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
            VariableValue::LocalTime(NaiveTime::from_hms_opt(12, 54, 3).unwrap())
        );
    }

    let expr = "localtime('125403')";
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
            VariableValue::LocalTime(NaiveTime::from_hms_opt(12, 54, 3).unwrap())
        );
    }
}

#[tokio::test]
async fn evalute_local_time_fraction() {
    let expr = "localtime('12:54:03.1234')";
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
            VariableValue::LocalTime(NaiveTime::from_hms_micro_opt(12, 54, 3, 123400).unwrap())
        );
    }

    let expr = "localtime('125403.1234')";
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
            VariableValue::LocalTime(NaiveTime::from_hms_micro_opt(12, 54, 3, 123400).unwrap())
        );
    }
}

#[tokio::test]
async fn evalute_local_time_hh_mm() {
    let expr = "localtime('12:54')";
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
            VariableValue::LocalTime(NaiveTime::from_hms_opt(12, 54, 0).unwrap())
        );
    }
}

#[tokio::test]
async fn evalute_zoned_time_hh_mm_utc() {
    let expr = "time('12:54:51Z')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_opt(12, 54, 51).unwrap();
        let offset = FixedOffset::east_opt(0).unwrap();

        let zoned_time = ZonedTime::new(naive_time, offset);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(zoned_time)
        );
    }
}

#[tokio::test]
async fn evalute_zoned_time_hh_mm_offset() {
    let expr = "time('12:54:51+03:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_opt(12, 54, 51).unwrap();
        let offset = FixedOffset::east_opt(10800).unwrap();

        let zoned_time = ZonedTime::new(naive_time, offset);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(zoned_time)
        );
    }

    let expr = "time('12:54:51-12:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_opt(12, 54, 51).unwrap();
        let offset = FixedOffset::west_opt(43200).unwrap();

        let zoned_time = ZonedTime::new(naive_time, offset);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(zoned_time)
        );
    }
}

#[tokio::test]
async fn evalute_zoned_time_hh_mm_frac_offset() {
    let expr = "time('12:54:51.1234+03:00')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 123400).unwrap();
        let offset = FixedOffset::east_opt(10800).unwrap();

        let zoned_time = ZonedTime::new(naive_time, offset);
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(zoned_time)
        );
    }
}

#[tokio::test]
async fn test_local_time_property_hour() {
    let expr = "$param1.hour";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(12, 31, 41).unwrap()),
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
async fn test_local_time_property_minute() {
    let expr = "$param1.minute";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(12, 31, 41).unwrap()),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(31.into())
        );
    }
}

#[tokio::test]
async fn test_local_time_property_second() {
    let expr = "$param1.second";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(12, 31, 41).unwrap()),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(41.into())
        );
    }
}

#[tokio::test]
async fn test_local_time_property_millisecond() {
    let expr = "$param1.millisecond";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(12, 31, 41, 123).unwrap()),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(123.into())
        );
    }
}

#[tokio::test]
async fn test_local_time_property_microsecond() {
    let expr = "$param1.microsecond";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::LocalTime(NaiveTime::from_hms_micro_opt(12, 31, 41, 123456).unwrap()),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(123456.into())
        );
    }
}

#[tokio::test]
async fn test_local_time_property_nanosecond() {
    let expr = "$param1.nanosecond";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(12, 31, 41, 123456789).unwrap()),
    );

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(123456789.into())
        );
    }
}

#[tokio::test]
async fn test_time_property_hour() {
    let expr = "$param1.hour";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_opt(12, 54, 51).unwrap();
    let offset = FixedOffset::east_opt(0).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

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
async fn test_time_property_minute() {
    let expr = "$param1.minute";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_opt(12, 54, 51).unwrap();
    let offset = FixedOffset::east_opt(0).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(54.into())
        );
    }
}

#[tokio::test]
async fn test_time_property_second() {
    let expr = "$param1.second";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_opt(12, 54, 51).unwrap();
    let offset = FixedOffset::east_opt(0).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(51.into())
        );
    }
}

#[tokio::test]
async fn test_time_property_microsecond() {
    let expr = "$param1.microsecond";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 1234).unwrap();
    let offset = FixedOffset::east_opt(0).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(1234.into())
        );
    }
}

#[tokio::test]
async fn test_time_property_offset() {
    let expr = "$param1.offset";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 1234).unwrap();
    let offset = FixedOffset::east_opt(3600).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("+01:00".to_string())
        );
    }
}

#[tokio::test]
async fn test_time_property_timezone() {
    let expr = "$param1.timezone";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 1234).unwrap();
    let offset = FixedOffset::east_opt(3600).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::String("+01:00".to_string())
        );
    }
}

#[tokio::test]
async fn test_time_property_offset_second() {
    let expr = "$param1.offsetSeconds";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 1234).unwrap();
    let offset = FixedOffset::east_opt(36230).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(36230.into())
        );
    }

    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 1234).unwrap();
    let offset = FixedOffset::west_opt(36230).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer((-36230).into())
        );
    }
}

#[tokio::test]
async fn test_time_property_offset_minute() {
    let expr = "$param1.offsetMinutes";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let naive_time = NaiveTime::from_hms_micro_opt(12, 54, 51, 1234).unwrap();
    let offset = FixedOffset::east_opt(36230).unwrap();
    let zoned_time = ZonedTime::new(naive_time, offset);

    variables.insert("param1".into(), VariableValue::ZonedTime(zoned_time));

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(603.into())
        );
    }
}

#[tokio::test]
async fn test_evaluate_zoned_time_realtime() {
    let expr = "time.realtime()";
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
            VariableValue::ZonedTime(ZonedTime::new(
                NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                FixedOffset::east_opt(0).unwrap()
            ))
        );
    }

    let expr = "time.realtime('America/Los Angeles')";
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
            VariableValue::ZonedTime(ZonedTime::new(
                NaiveTime::from_hms_opt(16, 0, 0).unwrap(),
                FixedOffset::west_opt(28800).unwrap()
            ))
        );
    }
}

#[tokio::test]
async fn test_local_time_creation_from_component() {
    let expr = "localtime({hour: 12, minute: 31, second: 14, nanosecond: 789, millisecond: 123, microsecond: 456})";
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
            VariableValue::LocalTime(NaiveTime::from_hms_nano_opt(12, 31, 14, 123456789).unwrap())
        );
    }
}

#[tokio::test]
async fn test_zoned_time_creation_from_component() {
    let expr = "time({hour: 12, minute: 31, second: 14, millisecond: 123, microsecond: 456, nanosecond: 789})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_time = NaiveTime::from_hms_nano_opt(12, 31, 14, 123456789).unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(ZonedTime::new(
                naive_time,
                FixedOffset::east_opt(0).unwrap()
            ))
        );
    }
}

#[tokio::test]
async fn test_zoned_time_creation_from_component_with_timezone() {
    let expr = "time({hour: 12, minute: 31, timezone: '+01:00'})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let naive_time = NaiveTime::from_hms_nano_opt(12, 31, 0, 0).unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(ZonedTime::new(
                naive_time,
                FixedOffset::east_opt(3600).unwrap()
            ))
        );
    }
}

#[tokio::test]
async fn test_local_time_duration_addition() {
    let expr =
        "localtime('12:31:14') + duration({hours: 1, minutes: 2, seconds: 3, milliseconds: 4})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_milli_opt(13, 33, 17, 4).unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalTime(naive_time)
        );
    }
}

#[tokio::test]
async fn test_zoned_time_duration_addition() {
    let expr =
        "time('12:31:14+01:00') + duration({hours: 1, minutes: 2, seconds: 3, milliseconds: 4})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_milli_opt(13, 33, 17, 4).unwrap();
        let offset = FixedOffset::east_opt(3600).unwrap();
        let zoned_time = ZonedTime::new(naive_time, offset);

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(zoned_time)
        );
    }
}

#[tokio::test]
async fn test_local_time_duration_subtraction() {
    let expr =
        "localtime('12:31:14') - duration({hours: 1, minutes: 2, seconds: 3, milliseconds: 4})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_milli_opt(11, 29, 10, 996).unwrap();
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::LocalTime(naive_time)
        );
    }
}

#[tokio::test]
async fn test_zoned_time_duration_subtraction() {
    let expr =
        "time('12:31:14-01:00') - duration({hours: 1, minutes: 2, seconds: 3, milliseconds: 4})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let naive_time = NaiveTime::from_hms_milli_opt(11, 29, 10, 996).unwrap();
        let offset = FixedOffset::west_opt(3600).unwrap();
        let zoned_time = ZonedTime::new(naive_time, offset);

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::ZonedTime(zoned_time)
        );
    }
}

#[tokio::test]
async fn test_local_time_lt() {
    let expr = "localtime('12:31:14') < localtime('13:33:17')";
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
async fn test_local_time_le() {
    let expr = "localtime('12:31:14') <= localtime('13:33:17')";
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
async fn test_local_time_ge() {
    let expr = "localtime('13:33:17') >= localtime('12:31:14')";
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

    {
        let expr = "localtime('13:33:17') >= localtime('12:33:17')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
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
async fn test_local_time_gt() {
    let expr = "localtime('13:33:17') > localtime('12:31:14')";
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
async fn test_zoned_time_lt() {
    let expr = "time('12:00:00Z') < time('13:00:00Z')";
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
async fn test_zoned_time_le() {
    let expr = "time('12:00:00Z') <= time('13:00:00Z')";
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

    {
        let expr = "time('12:00:00Z') <= time('12:00:00Z')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
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
async fn test_zoned_time_gt() {
    let expr = "time('13:00:00Z') > time('12:00:00Z')";
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
async fn test_zoned_time_ge() {
    let expr = "time('13:00:00Z') >= time('12:00:00Z')";
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
async fn evaluate_local_time_empty_param() {
    let expr = "localtime()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let curr_time = Local::now().time();
        let result = match evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap()
        {
            VariableValue::LocalTime(result) => result,
            _ => panic!("Failed to get local time"),
        };
        // ensure that the result is within 500ms of the current time
        assert!(
            curr_time
                .signed_duration_since(result)
                .num_milliseconds()
                .abs()
                < 500
        );
    }
}

#[tokio::test]
async fn test_local_time_transaction() {
    let expr = "localtime.transaction()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
}

#[tokio::test]
async fn test_local_time_statement() {
    let expr = "localtime.statement()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
}

#[tokio::test]
async fn test_local_time_realtime() {
    let expr = "localtime.realtime()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = match evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap()
        {
            VariableValue::LocalTime(result) => result,
            _ => panic!("Failed to get local time"),
        };
        // ensure that the result is within 500ms of the current time
        assert_eq!(result, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    }
}

#[tokio::test]
async fn test_create_local_time_with_components() {
    let expr = "localtime({hour: 12, minute: 31, second: 14, millisecond: 123, microsecond: 456, nanosecond: 789})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(
        result,
        NaiveTime::from_hms_nano_opt(12, 31, 14, 123456789).unwrap()
    );

    let expr = "localtime({hour: 12})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_opt(12, 0, 0).unwrap());
}

#[tokio::test]
async fn test_local_time_truncate() {
    let naive_time = NaiveTime::from_hms_nano_opt(12, 31, 14, 645876123).unwrap();
    let local_time = VariableValue::LocalTime(naive_time);

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    variables.insert("param1".to_string().into(), local_time);
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let expr = "localtime.truncate('day', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_opt(0, 0, 0).unwrap());

    let expr = "localtime.truncate('hour', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_opt(12, 0, 0).unwrap());

    let expr = "localtime.truncate('minute', $param1, {millisecond: 2})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_milli_opt(12, 31, 0, 2).unwrap());

    let expr = "localtime.truncate('second', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(result, NaiveTime::from_hms_opt(12, 31, 14).unwrap());

    let expr = "localtime.truncate('millisecond', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(
        result,
        NaiveTime::from_hms_milli_opt(12, 31, 14, 645).unwrap()
    );

    let expr = "localtime.truncate('microsecond', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    let result = match result {
        VariableValue::LocalTime(result) => result,
        _ => panic!("Failed to get local time"),
    };
    assert_eq!(
        result,
        NaiveTime::from_hms_micro_opt(12, 31, 14, 645876).unwrap()
    );
}

#[tokio::test]
async fn evaluate_zoned_time_empty_param() {
    let expr = "time()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let curr_time = Local::now().time();
        let result = match evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap()
        {
            VariableValue::ZonedTime(result) => result,
            _ => panic!("Failed to get zoned date time"),
        };
        // ensure that the result is within 500ms of the current time
        assert!(
            curr_time
                .signed_duration_since(*result.time())
                .num_milliseconds()
                .abs()
                < 500
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_time_with_timezone() {
    let expr = "time({timezone: 'Asia/Shanghai'})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = match evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap()
        {
            VariableValue::ZonedTime(result) => result,
            _ => panic!("Failed to get zoned date time"),
        };

        let current_time_in_shanghai = Local::now()
            .with_timezone(&FixedOffset::east_opt(28800).unwrap())
            .time();
        assert!(
            current_time_in_shanghai
                .signed_duration_since(*result.time())
                .num_milliseconds()
                .abs()
                < 500
        );
    }
}

#[tokio::test]
async fn evaluate_zoned_time_statement() {
    let expr = "time.statement()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = match evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap()
        {
            VariableValue::ZonedTime(result) => result,
            _ => panic!("Failed to get zoned date time"),
        };
        assert_eq!(result.time(), &NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    }
}

#[tokio::test]
async fn evaluate_zoned_time_transaction() {
    let expr = "time.transaction()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = match evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap()
        {
            VariableValue::ZonedTime(result) => result,
            _ => panic!("Failed to get zoned date time"),
        };
        assert_eq!(result.time(), &NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    }
}

#[tokio::test]
async fn test_zoned_time_truncate() {
    let time = NaiveTime::from_hms_nano_opt(12, 31, 14, 645876123).unwrap();
    let fixed_offset = FixedOffset::from_str("-01:00").unwrap();

    let zoned_time = VariableValue::ZonedTime(ZonedTime::new(time, fixed_offset));
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    variables.insert("param1".to_string().into(), zoned_time);

    let expr = "time.truncate('day', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedTime(result)) => result,
        _ => panic!("Failed to get zoned time"),
    };
    assert_eq!(result.time(), &NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    assert_eq!(result.offset(), &FixedOffset::west_opt(3600).unwrap());

    let expr = "time.truncate('hour', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedTime(result)) => result,
        _ => panic!("Failed to get zoned time"),
    };
    assert_eq!(result.time(), &NaiveTime::from_hms_opt(12, 0, 0).unwrap());
    assert_eq!(result.offset(), &FixedOffset::west_opt(3600).unwrap());

    let expr = "time.truncate('minute', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedTime(result)) => result,
        _ => panic!("Failed to get zoned time"),
    };
    assert_eq!(result.time(), &NaiveTime::from_hms_opt(12, 31, 0).unwrap());
    assert_eq!(result.offset(), &FixedOffset::west_opt(3600).unwrap());

    let expr = "time.truncate('second', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedTime(result)) => result,
        _ => panic!("Failed to get zoned time"),
    };
    assert_eq!(result.time(), &NaiveTime::from_hms_opt(12, 31, 14).unwrap());
    assert_eq!(result.offset(), &FixedOffset::west_opt(3600).unwrap());

    let expr = "time.truncate('millisecond', $param1, {nanosecond: 2})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedTime(result)) => result,
        _ => panic!("Failed to get zoned time"),
    };
    assert_eq!(
        result.time(),
        &NaiveTime::from_hms_nano_opt(12, 31, 14, 645000002).unwrap()
    );
    assert_eq!(result.offset(), &FixedOffset::west_opt(3600).unwrap());

    let expr = "time.truncate('microsecond', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(VariableValue::ZonedTime(result)) => result,
        _ => panic!("Failed to get zoned time"),
    };
    assert_eq!(
        result.time(),
        &NaiveTime::from_hms_micro_opt(12, 31, 14, 645876).unwrap()
    );
    assert_eq!(result.offset(), &FixedOffset::west_opt(3600).unwrap());
}
