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

use chrono::NaiveDate;
use drasi_query_ast::ast::{self, QueryPart};
use mockall::predicate::*;
use mockall::*;
use serde_json::json;

use crate::evaluation::context::{ChangeContext, QueryVariables};
use crate::evaluation::functions::future::true_now_or_later::TrueNowOrLater;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};
use crate::interface::{FutureQueue, IndexError};
use crate::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};

// Create a mock for FutureQueue
mock! {
    pub FutureQueue {}

    #[async_trait::async_trait]
    impl FutureQueue for FutureQueue {
        async fn push(
            &self,
            push_type: crate::interface::PushType,
            position_in_query: usize,
            group_signature: u64,
            element_ref: &crate::models::ElementReference,
            original_time: crate::models::ElementTimestamp,
            due_time: crate::models::ElementTimestamp,
        ) -> Result<bool, IndexError>;

        async fn remove(
            &self,
            position_in_query: usize,
            group_signature: u64,
        ) -> Result<(), IndexError>;

        async fn pop(&self) -> Result<Option<crate::interface::FutureElementRef>, IndexError>;

        async fn peek_due_time(&self) -> Result<Option<crate::models::ElementTimestamp>, IndexError>;

        async fn clear(&self) -> Result<(), IndexError>;
    }
}

#[tokio::test]
async fn test_true_now_or_later_invalid_args_count() {
    // Setup
    let mock_queue = MockFutureQueue::new();
    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // Test with no arguments (should require 2)
    let result = function.call(&context, &get_func_expr(), vec![]).await;

    // Verify
    assert!(result.is_err());
    if let Err(FunctionError {
        function_name,
        error,
    }) = result
    {
        assert_eq!(function_name, "function");
        assert!(matches!(
            error,
            FunctionEvaluationError::InvalidArgumentCount
        ));
    }
}

#[tokio::test]
async fn test_true_now_or_later_condition_true() {
    // Setup
    let mock_queue = MockFutureQueue::new();
    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let anchor = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::new([Arc::from("TestLabel")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(
            json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
        ),
    };

    let binding = QueryVariables::new();
    let part = QueryPart::default();
    let context = ExpressionEvaluationContext::from_after_change(
        &binding,
        &ChangeContext {
            before_grouping_hash: 0,
            after_grouping_hash: 0,
            before_clock: Arc::new(InstantQueryClock::new(0, 0)),
            after_clock: Arc::new(InstantQueryClock::new(0, 0)),
            solution_signature: 0,
            before_anchor_element: None,
            after_anchor_element: Some(Arc::new(anchor)),
            is_future_reprocess: false,
        },
        &part,
    );

    // Test when condition is true
    let result = function
        .call(
            &context,
            &get_func_expr(),
            vec![
                VariableValue::Bool(true),
                VariableValue::Integer(100.into()),
            ],
        )
        .await;

    // Verify - should return true immediately without scheduling
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), VariableValue::Bool(true));
}

#[tokio::test]
async fn test_true_now_or_later_due_time_passed() {
    // Setup
    let mock_queue = MockFutureQueue::new();
    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let anchor = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::new([Arc::from("TestLabel")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(
            json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
        ),
    };

    let current_time = 1000;
    let due_time = 900; // Earlier than current time

    let binding = QueryVariables::new();
    let part = QueryPart::default();
    let context = ExpressionEvaluationContext::from_after_change(
        &binding,
        &ChangeContext {
            before_grouping_hash: 0,
            after_grouping_hash: 0,
            before_clock: Arc::new(InstantQueryClock::new(current_time, current_time)),
            after_clock: Arc::new(InstantQueryClock::new(current_time, current_time)),
            solution_signature: 0,
            before_anchor_element: None,
            after_anchor_element: Some(Arc::new(anchor)),
            is_future_reprocess: false,
        },
        &part,
    );

    // Test when due time has already passed
    let result = function
        .call(
            &context,
            &get_func_expr(),
            vec![
                VariableValue::Bool(false),
                VariableValue::Integer(due_time.into()),
            ],
        )
        .await;

    // Verify - should return the condition value (false) without scheduling
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_true_now_or_later_schedule_future() {
    // Setup
    let current_time = 1000;
    let due_time = 1500; // Later than current time

    let mut mock_queue = MockFutureQueue::new();

    // Expect push to be called with specific parameters
    mock_queue
        .expect_push()
        .with(
            eq(crate::interface::PushType::Overwrite),
            eq(10),           // position_in_query from get_func_expr
            always(),         // group_signature
            always(),         // element_ref
            eq(current_time), // original_time
            eq(due_time),     // due_time
        )
        .returning(|_, _, _, _, _, _| Ok(true));

    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let anchor = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::new([Arc::from("TestLabel")]),
            effective_from: 2000,
        },
        properties: ElementPropertyMap::from(
            json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
        ),
    };

    let binding = QueryVariables::new();
    let part = QueryPart::default();
    let context = ExpressionEvaluationContext::from_after_change(
        &binding,
        &ChangeContext {
            before_grouping_hash: 0,
            after_grouping_hash: 123, // Different hash for testing
            before_clock: Arc::new(InstantQueryClock::new(current_time, current_time)),
            after_clock: Arc::new(InstantQueryClock::new(current_time, current_time)),
            solution_signature: 0,
            before_anchor_element: None,
            after_anchor_element: Some(Arc::new(anchor)),
            is_future_reprocess: false,
        },
        &part,
    );

    // Test scheduling for future evaluation
    let result = function
        .call(
            &context,
            &get_func_expr(),
            vec![
                VariableValue::Bool(false),
                VariableValue::Integer(due_time.into()),
            ],
        )
        .await;

    // Verify - should return Awaiting
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), VariableValue::Awaiting);
}

#[tokio::test]
async fn test_true_now_or_later_with_date() {
    // Setup
    let mut mock_queue = MockFutureQueue::new();

    // Calculate expected timestamp
    let date = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
    let expected_timestamp = date
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis() as u64;

    mock_queue
        .expect_push()
        .with(
            eq(crate::interface::PushType::Overwrite),
            eq(10),
            always(),
            always(),
            eq(1000),
            eq(expected_timestamp),
        )
        .returning(|_, _, _, _, _, _| Ok(true));

    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let anchor = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::new([Arc::from("TestLabel")]),
            effective_from: 2000,
        },
        properties: ElementPropertyMap::from(
            json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
        ),
    };

    let binding = QueryVariables::new();
    let part = QueryPart::default();
    let context = ExpressionEvaluationContext::from_after_change(
        &binding,
        &ChangeContext {
            before_grouping_hash: 0,
            after_grouping_hash: 123,
            before_clock: Arc::new(InstantQueryClock::new(1000, 2000)),
            after_clock: Arc::new(InstantQueryClock::new(1000, 2000)),
            solution_signature: 0,
            before_anchor_element: None,
            after_anchor_element: Some(Arc::new(anchor)),
            is_future_reprocess: false,
        },
        &part,
    );

    // Test with Date argument
    let result = function
        .call(
            &context,
            &get_func_expr(),
            vec![VariableValue::Bool(false), VariableValue::Date(date)],
        )
        .await;

    // Verify
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), VariableValue::Awaiting);
}

#[tokio::test]
async fn test_true_now_or_later_invalid_condition_type() {
    // Setup
    let mock_queue = MockFutureQueue::new();
    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let anchor = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::new([Arc::from("TestLabel")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(
            json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
        ),
    };

    let binding = QueryVariables::new();
    let part = QueryPart::default();
    let context = ExpressionEvaluationContext::from_after_change(
        &binding,
        &ChangeContext {
            before_grouping_hash: 0,
            after_grouping_hash: 0,
            before_clock: Arc::new(InstantQueryClock::new(0, 0)),
            after_clock: Arc::new(InstantQueryClock::new(0, 0)),
            solution_signature: 0,
            before_anchor_element: None,
            after_anchor_element: Some(Arc::new(anchor)),
            is_future_reprocess: false,
        },
        &part,
    );

    // Test with invalid condition type (string instead of bool)
    let result = function
        .call(
            &context,
            &get_func_expr(),
            vec![
                VariableValue::String("not a bool".to_string()),
                VariableValue::Integer(100.into()),
            ],
        )
        .await;

    // Verify
    assert!(result.is_err());
    if let Err(FunctionError {
        function_name,
        error,
    }) = result
    {
        assert_eq!(function_name, "function");
        assert!(matches!(error, FunctionEvaluationError::InvalidArgument(0)));
    }
}

#[tokio::test]
async fn test_true_now_or_later_null_arguments() {
    // Setup
    let mock_queue = MockFutureQueue::new();
    let function = TrueNowOrLater::new(Arc::new(mock_queue));

    let anchor = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_namespace", "test_id"),
            labels: Arc::new([Arc::from("TestLabel")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(
            json!({ "invoice_id": "invoice_01", "timestamp": 1696204800, "status": "overdue" }),
        ),
    };

    let binding = QueryVariables::new();
    let part = QueryPart::default();
    let context = ExpressionEvaluationContext::from_after_change(
        &binding,
        &ChangeContext {
            before_grouping_hash: 0,
            after_grouping_hash: 0,
            before_clock: Arc::new(InstantQueryClock::new(0, 0)),
            after_clock: Arc::new(InstantQueryClock::new(0, 0)),
            solution_signature: 0,
            before_anchor_element: None,
            after_anchor_element: Some(Arc::new(anchor)),
            is_future_reprocess: false,
        },
        &part,
    );

    // Test with null condition
    let result1 = function
        .call(
            &context,
            &get_func_expr(),
            vec![VariableValue::Null, VariableValue::Integer(100.into())],
        )
        .await;

    // Test with null due time
    let result2 = function
        .call(
            &context,
            &get_func_expr(),
            vec![VariableValue::Bool(false), VariableValue::Null],
        )
        .await;

    // Verify both return null
    assert_eq!(result1.unwrap(), VariableValue::Null);
    assert_eq!(result2.unwrap(), VariableValue::Null);
}

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}
