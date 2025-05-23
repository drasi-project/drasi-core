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
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};
use chrono::NaiveDate;
use drasi_query_ast::ast;
use std::sync::Arc;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_parse_date() {
    let subject = temporal_instant::Date {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("2025-01-01".to_string()),
    ];
    let result = subject
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::Date(NaiveDate::from_ymd_opt(2025, 1, 1).unwrap())
    );
}


#[tokio::test]
async fn test_parse_zoned_datetime() {
    let subject = temporal_instant::DateTime {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = subject
        .call(&context, &get_func_expr(), vec![
        //VariableValue::String("2025-01-01T12:00:00".to_string()),
        VariableValue::String("2015-07-21T21:40:32.142+01:00".to_string()),
    ])
        .await;

    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(
            chrono::DateTime::parse_from_rfc3339("2015-07-21T21:40:32.142+01:00").unwrap(),
            None,
        ))
    );

    let result = subject
        .call(&context, &get_func_expr(), vec![
        VariableValue::String("2025-01-01T12:00:00Z".to_string()),        
    ])
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(
            chrono::DateTime::parse_from_rfc3339("2025-01-01T12:00:00+00:00").unwrap(),
            None,
        ))
    );

    let result = subject
        .call(&context, &get_func_expr(), vec![
        VariableValue::String("2025-01-01T12:00Z".to_string()),        
    ])
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::ZonedDateTime(ZonedDateTime::new(
            chrono::DateTime::parse_from_rfc3339("2025-01-01T12:00:00+00:00").unwrap(),
            None,
        ))
    );
}
