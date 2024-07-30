use chrono::NaiveDate;
use chrono::prelude::*;
use std::sync::Arc;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::temporal_constants;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::evaluation::functions::FunctionRegistry;
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;


#[tokio::test]
async fn evaluate_date_empty() {
    let expr = "date()";
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
            {
                let local = Local::now().date_naive();
                VariableValue::Date(local)
            }
        );
    }
}

#[tokio::test]
async fn evalute_local_time_yy_mm_dd() {
    let expr = "date('2020-11-04')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap())
        );
    }

    let expr = "date('20201104')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap())
        );
    }
}

#[tokio::test]
async fn evalute_local_time_yy_mm() {
    let expr = "date('2020-11')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 1).unwrap())
        );
    }

    let expr = "date('202011')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 1).unwrap())
        );
    }
}

#[tokio::test]
async fn evalute_local_time_yy_ww_dd() {
    let expr = "date('2015-W30-2')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2015, 7, 21).unwrap())
        );
    }

    let expr = "date('2015W302')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2015, 7, 21).unwrap())
        );
    }
}

#[tokio::test]
async fn evalute_local_time_yy_ww() {
    let expr = "date('2015-W30')";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(2015, 7, 20).unwrap())
        );
    }
}

#[tokio::test]
async fn test_date_property_year() {
    let expr = "$param1.year";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
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
async fn test_date_property_month() {
    let expr = "$param1.month";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
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
async fn test_date_property_day() {
    let expr = "$param1.day";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 11, 4).unwrap()),
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
async fn test_date_property_quarter() {
    let expr = "$param1.quarter";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2020, 8, 4).unwrap()),
    );
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
async fn test_date_property_week() {
    let expr = "$param1.week";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2015, 7, 21).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(30.into())
        );
    }
}

#[tokio::test]
async fn test_date_property_day_of_week() {
    let expr = "$param1.dayOfWeek";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2015, 7, 21).unwrap()),
    );
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
async fn test_date_property_ordinal_day() {
    let expr = "$param1.ordinalDay";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2015, 7, 21).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(202.into())
        );
    }
}

#[tokio::test]
async fn test_date_property_day_of_quarter() {
    let expr = "$param1.dayOfQuarter";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2015, 5, 30).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Integer(60.into())
        );
    }
}

#[tokio::test]
async fn test_evaluate_date_truncate_month() {
    let expr = "date.truncate('month', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2015, 5, 30).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2015, 5, 1).unwrap())
        );
    }
}


#[tokio::test]
async fn test_evaluate_date_truncate_week() {
    let expr = "date.truncate('week', $param1, {dayOfWeek: 2})";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017,11,11).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2017,11,7).unwrap())
        );
    }

}

#[tokio::test]
async fn test_truncate_with_millennium() {
    let expr = "date.truncate('millennium', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2015, 5, 30).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap())
        );
    }

}


#[tokio::test]
async fn test_evaluate_date_truncate_weekyear() {
    let expr = "date.truncate('weekyear', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017,11,11).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2017,1,2).unwrap())
        );
    }


}

#[tokio::test]
async fn test_evaluate_date_truncate_quarter() {
    let expr = "date.truncate('quarter', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017,11,11).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2017,10,1).unwrap())
        );
    }

}

#[tokio::test]
async fn test_evaluate_date_truncate_day() {
    let expr = "date.truncate('day', $param1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert(
        "param1".into(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2017,11,11).unwrap()),
    );
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2017,11,11).unwrap())
        );
    }


}

#[tokio::test]
async fn test_evaluate_date_realtime() {
    let expr = "date.realtime()";
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
            VariableValue::Date(*temporal_constants::EPOCH_NAIVE_DATE)
        );
    }
}

#[tokio::test]
async fn test_date_creation_from_component() {
    let expr = "date({year: 1984, month: 10, day: 11})";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(1984, 10, 11).unwrap())
        );
    }
}

#[tokio::test]
async fn test_date_creation_from_component_quarter() {
    let expr = "date({year: 1984, quarter: 3, dayOfQuarter: 45})";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(1984, 8, 14).unwrap())
        );
    }
}

#[tokio::test]
async fn test_date_creation_from_component_week() {
    let expr = "date({year: 1984, week: 10, dayOfWeek: 3})";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(1984, 3, 7).unwrap())
        );
    }
}

#[tokio::test]
async fn test_date_creation_from_component_ordinal() {
    let expr = "date({year: 1984, ordinalDay: 202})";
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
            VariableValue::Date(NaiveDate::from_ymd_opt(1984, 7, 20).unwrap())
        );
    }
}

#[tokio::test]
async fn test_access_date_component_from_function() {
    let expr = "date({year: 1984, ordinalDay: 202}).year";
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
            VariableValue::Integer(1984.into())
        );
    }
}

#[tokio::test]
async fn test_date_duration_addition() {
    let expr = "date('2020-08-04') + duration('P3D')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2020, 8, 7).unwrap())
        );
    }
}

#[tokio::test]
async fn test_date_duration_subtraction() {
    let expr = "date('2020-08-04') - duration('P13D')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());

    let ari = Arc::new(InMemoryResultIndex::new());

    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(NaiveDate::from_ymd_opt(2020, 7, 22).unwrap())
        );
    }
}

#[tokio::test]
async fn test_date_lt() {
    let expr = "date('2020-08-04') < date('2020-08-05')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
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
        let expr = "date('2020-08-04') < datetime('2020-08-02T00:00:00Z')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }
}

#[tokio::test]
async fn test_date_le() {
    let expr = "date('2020-08-04') <= date('2020-08-05')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
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
        let expr = "date('2020-08-04') <= datetime('2020-08-02T00:00:00Z')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }

    {
        let expr = "date('2020-08-04') <= date('2020-08-04')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
        let variables = QueryVariables::new();
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
async fn test_date_gt() {
    let expr = "date('2020-08-04') > date('2020-08-03')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
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
        let expr = "date('2020-08-04') > datetime('2020-08-05T00:00:00Z')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }
}

#[tokio::test]
async fn test_date_ge() {
    let expr = "date('2020-08-04') >= date('2020-08-03')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
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
        let expr = "date('2020-08-04') >= datetime('2020-08-05T00:00:00Z')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }

    {
        let expr = "date('2020-08-04') >= date('2020-08-04')";
        let expr = drasi_query_cypher::parse_expression(expr).unwrap();
        let variables = QueryVariables::new();
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
async fn test_retrieve_current_date() {
    let expr = "date()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    {
        let variables = QueryVariables::new();
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Date(Local::now().date_naive())
        );
    }
}