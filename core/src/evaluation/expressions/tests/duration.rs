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

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::functions::{
    Between, Date, DateTime, DurationFunc, Function, InDays, InMonths, InSeconds,
};
use crate::evaluation::variable_value::duration::Duration;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDate, NaiveTime};
use std::sync::Arc;

fn create_duration_expression_test_function_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());

    registry.register_function("duration", Function::Scalar(Arc::new(DurationFunc {})));
    registry.register_function("duration.between", Function::Scalar(Arc::new(Between {})));
    registry.register_function("duration.inDays", Function::Scalar(Arc::new(InDays {})));
    registry.register_function("duration.inMonths", Function::Scalar(Arc::new(InMonths {})));
    registry.register_function(
        "duration.inSeconds",
        Function::Scalar(Arc::new(InSeconds {})),
    );

    // Also need date/datetime functions for some tests
    registry.register_function("date", Function::Scalar(Arc::new(Date {})));
    registry.register_function("datetime", Function::Scalar(Arc::new(DateTime {})));

    registry
}

#[tokio::test]
async fn test_duration_standard() {
    let expr = "duration('P1Y2M3DT4H5M6S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let chrono_duration = ChronoDuration::days(3)
            + ChronoDuration::hours(4)
            + ChronoDuration::minutes(5)
            + ChronoDuration::seconds(6);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            duration
        );
    }
}

#[tokio::test]
async fn test_duration_frac_seconds() {
    let expr = "duration('P1Y2M7DT4H25M11.4S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let chrono_duration = ChronoDuration::days(7)
            + ChronoDuration::hours(4)
            + ChronoDuration::minutes(25)
            + ChronoDuration::seconds(11)
            + ChronoDuration::milliseconds(400);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            duration
        );
    }
}

#[tokio::test]
async fn test_duration_frac_days() {
    let expr = "duration('P1Y2M3.5DT4H5M6S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let chrono_duration = ChronoDuration::days(3)
            + ChronoDuration::hours(4)
            + ChronoDuration::minutes(5)
            + ChronoDuration::seconds(6)
            + ChronoDuration::hours(12);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            duration
        );
    }
}

#[tokio::test]
async fn test_duration_property_year() {
    let expr = "$param1.years";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::days(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::minutes(5)
        + ChronoDuration::seconds(6);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 12));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(2.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_months() {
    let expr = "$param1.months";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::days(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::minutes(5)
        + ChronoDuration::seconds(6);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 12));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(24.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_weeks() {
    let expr = "$param1.weeks";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::weeks(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::days(11)
        + ChronoDuration::seconds(6);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));

    variables.insert("param1".into(), duration);
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
async fn test_duration_property_days() {
    let expr = "$param1.days";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::weeks(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::days(11)
        + ChronoDuration::seconds(6);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(32.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_hours() {
    let expr = "$param1.hours";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::weeks(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::seconds(3716)
        + ChronoDuration::minutes(121);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(511.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_quarters() {
    let expr = "$param1.quarters";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::weeks(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::seconds(3716)
        + ChronoDuration::minutes(121);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));

    variables.insert("param1".into(), duration);
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
async fn test_duration_property_seconds() {
    let expr = "$param1.seconds";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let chrono_duration = ChronoDuration::weeks(3)
        + ChronoDuration::hours(4)
        + ChronoDuration::seconds(3716)
        + ChronoDuration::minutes(121);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(1839776.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_microseconds() {
    let expr = "$param1.microseconds";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    let chrono_duration =
        ChronoDuration::hours(3) + ChronoDuration::seconds(3716) + ChronoDuration::minutes(121);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 0));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(21776000000_i64.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_months_of_year() {
    let expr = "$param1.monthsOfYear";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    let chrono_duration =
        ChronoDuration::hours(3) + ChronoDuration::seconds(3716) + ChronoDuration::minutes(121);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 14));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(2.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_months_of_quarter() {
    let expr = "$param1.monthsOfQuarter";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let chrono_duration =
        ChronoDuration::hours(3) + ChronoDuration::seconds(3716) + ChronoDuration::minutes(121);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 9));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(3.into())
        );
    }
}

#[tokio::test]
async fn test_duration_property_days_of_week() {
    let expr = "$param1.daysOfWeek";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let chrono_duration = ChronoDuration::days(25);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 5));

    variables.insert("param1".into(), duration);
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
async fn test_duration_property_minutes_of_hour() {
    let expr = "$param1.minutesOfHour";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();

    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let mut variables = QueryVariables::new();
    let chrono_duration = ChronoDuration::minutes(215);
    let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 5));

    variables.insert("param1".into(), duration);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(35.into())
        );
    }
}

#[tokio::test]
async fn test_evaluate_duration_between() {
    let expr = "duration.between($param1, $param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 3, 4).unwrap());
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2021, 5, 20).unwrap());
    variables.insert("param1".into(), date1);
    variables.insert("param2".into(), date2);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration = ChronoDuration::days(442);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 0));
        assert_eq!(result, duration);
    }

    let time1 = VariableValue::ZonedTime(ZonedTime::new(
        NaiveTime::from_hms_opt(1, 2, 3).unwrap(),
        FixedOffset::east_opt(7200).unwrap(),
    ));
    let datetime2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2021, 5, 20)
            .unwrap()
            .and_hms_opt(12, 41, 23)
            .unwrap(),
    );
    variables.insert("param1".into(), time1);
    variables.insert("param2".into(), datetime2);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration =
            ChronoDuration::hours(11) + ChronoDuration::minutes(39) + ChronoDuration::seconds(20);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 0));
        assert_eq!(result, duration);
    }
}

#[tokio::test]
async fn test_evaluate_duration_inmonths() {
    let expr = "duration.inMonths($param1, $param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let date1 = VariableValue::Date(NaiveDate::from_ymd_opt(2020, 3, 4).unwrap());
    let date2 = VariableValue::Date(NaiveDate::from_ymd_opt(2018, 5, 20).unwrap());
    variables.insert("param1".into(), date1);
    variables.insert("param2".into(), date2);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration = ChronoDuration::days(0);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, -1, -10));
        assert_eq!(result, duration);
    }
}

#[tokio::test]
async fn test_evaluate_duration_indays() {
    let expr = "duration.inDays($param1, $param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let date1 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2020, 3, 24)
            .unwrap()
            .and_hms_opt(1, 2, 3)
            .unwrap(),
    );
    let date2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2018, 5, 20)
            .unwrap()
            .and_hms_opt(12, 41, 23)
            .unwrap(),
    );
    variables.insert("param1".into(), date1);
    variables.insert("param2".into(), date2);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration = ChronoDuration::days(-673);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 0));
        assert_eq!(result, duration);
    }
}

#[tokio::test]
async fn test_evaluate_duration_in_seconds() {
    let expr = "duration.inSeconds($param1, $param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let date1 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2020, 3, 24)
            .unwrap()
            .and_hms_opt(1, 2, 3)
            .unwrap(),
    );
    let date2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2018, 5, 20)
            .unwrap()
            .and_hms_opt(12, 41, 23)
            .unwrap(),
    );
    variables.insert("param1".into(), date1);
    variables.insert("param2".into(), date2);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration = ChronoDuration::days(-673)
            + ChronoDuration::hours(-12)
            + ChronoDuration::minutes(-20)
            + ChronoDuration::seconds(-40);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 0));
        assert_eq!(result, duration);
    }
}

#[tokio::test]
async fn test_duration_creation_from_component() {
    let expr = "duration({years:1, minutes: 1.5, seconds: 1, milliseconds: 123, microseconds: 456, nanoseconds: 789})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let date1 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2020, 3, 24)
            .unwrap()
            .and_hms_opt(1, 2, 3)
            .unwrap(),
    );
    let date2 = VariableValue::LocalDateTime(
        NaiveDate::from_ymd_opt(2018, 5, 20)
            .unwrap()
            .and_hms_opt(12, 41, 23)
            .unwrap(),
    );
    variables.insert("param1".into(), date1);
    variables.insert("param2".into(), date2);
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration = ChronoDuration::minutes(1)
            + ChronoDuration::seconds(31)
            + ChronoDuration::milliseconds(123)
            + ChronoDuration::microseconds(456)
            + ChronoDuration::nanoseconds(789);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 0));
        assert_eq!(result, duration);
    }
}

#[tokio::test]
async fn test_duration_in_days_date_and_epoch() {
    let expr = "duration.inDays(date('2023-12-01'), datetime({epochSeconds: 1697661665}))";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let chrono_duration = ChronoDuration::days(-43);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 0, 0));
        assert_eq!(result, duration);
    }
}

#[tokio::test]
async fn test_accesing_components_from_functions() {
    let expr = "duration.inDays(date('2023-12-01'), datetime({epochSeconds: 1697661665})).days";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        let result = evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap();

        let expected_days = VariableValue::Integer((-43).into());
        assert_eq!(result, expected_days);
    }
}

#[tokio::test]
async fn test_duration_addition() {
    let expr =
        "duration('P1Y2M3DT4H5M6S') + duration({minutes: 1.5, seconds: 1, milliseconds: 123})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let chrono_duration = ChronoDuration::days(3)
            + ChronoDuration::hours(4)
            + ChronoDuration::minutes(6)
            + ChronoDuration::seconds(37)
            + ChronoDuration::milliseconds(123);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            duration
        );
    }
}

#[tokio::test]
async fn test_duration_subtraction() {
    let expr = "duration('P1Y2M3DT4H5M6S') - duration({days: 1, minutes: 1.5, seconds: 1, milliseconds: 123})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        let chrono_duration = ChronoDuration::days(2)
            + ChronoDuration::hours(4)
            + ChronoDuration::minutes(3)
            + ChronoDuration::seconds(34)
            + ChronoDuration::milliseconds(877);
        let duration = VariableValue::Duration(Duration::new(chrono_duration, 1, 2));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            duration
        );
    }
}

#[tokio::test]
async fn test_duration_comparison_less_than() {
    let expr = "duration('P1Y2M3DT4H5M6S') < duration('P1Y2M3DT4H5M7S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();
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
async fn test_duration_comparison_greater_than() {
    let expr = "duration('P1Y2M3DT4H5M6S') > duration('P1Y2M3DT4H5M5S')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = create_duration_expression_test_function_registry();
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
