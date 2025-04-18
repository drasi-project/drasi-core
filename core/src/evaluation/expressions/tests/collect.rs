// Copyright 2025 The Drasi Authors.
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

use std::{collections::HashSet, sync::Arc};

use drasi_query_ast::ast::{self, Literal, UnaryExpression};

use crate::{
    evaluation::{
        context::QueryVariables,
        functions::{
            aggregation::{Accumulator, Collect, ValueAccumulator},
            AggregatingFunction,
        },
        variable_value::{integer::Integer, VariableValue},
        ExpressionEvaluationContext, InstantQueryClock,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

// Helper to create a basic context
fn create_test_context() -> ExpressionEvaluationContext<'static> {
    // Create static variables for simplicity if QueryVariables::new() is const
    static EMPTY_VARIABLES: QueryVariables = QueryVariables::new();
    // Create the clock instance locally instead of using static
    let clock = InstantQueryClock::new(0, 0);
    // Wrap the local clock instance in Arc
    ExpressionEvaluationContext::new(&EMPTY_VARIABLES, Arc::new(clock))
}

// Helper to convert result list to HashSet for order-independent comparison
fn result_to_set(value: VariableValue) -> HashSet<VariableValue> {
    match value {
        VariableValue::List(list) => list.into_iter().collect::<HashSet<_>>(),
        other => panic!("Expected VariableValue::List, got {:?}", other),
    }
}

// Helper to create a dummy FunctionExpression (needed for initialize_accumulator)
fn dummy_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: "collect".to_string().into(),
        args: vec![
            ast::Expression::UnaryExpression(
                UnaryExpression::Literal(
                    Literal::Text("dummy".to_string().into())
                )
            )
        ],
        position_in_query: Default::default(),
    }
}

// Helper to create a dummy grouping key vec (needed for initialize_accumulator)
fn dummy_grouping_keys() -> Vec<VariableValue> {
    Vec::new()
}


#[tokio::test]
async fn test_collect_basic() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let val1 = VariableValue::Integer(Integer::from(10));
    let val2 = VariableValue::String("hello".to_string());
    let val3 = VariableValue::Bool(true);

    // Apply values and unwrap
    let res1 = collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply 1 failed");
    let res2 = collect_fn.apply(&context, vec![val2.clone()], &mut accumulator).await.expect("Apply 2 failed");
    let res3 = collect_fn.apply(&context, vec![val3.clone()], &mut accumulator).await.expect("Apply 3 failed");

    // Check intermediate results (order doesn't matter)
    assert_eq!(result_to_set(res1), HashSet::from_iter(vec![val1.clone()]));
    assert_eq!(result_to_set(res2), HashSet::from_iter(vec![val1.clone(), val2.clone()]));
    assert_eq!(result_to_set(res3), HashSet::from_iter(vec![val1.clone(), val2.clone(), val3.clone()]));

    // Check final snapshot and unwrap
    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::from_iter(vec![val1, val2, val3]));
}

#[tokio::test]
async fn test_collect_with_duplicates() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let val1 = VariableValue::Integer(Integer::from(5));
    let val2 = VariableValue::String("test".to_string());

    collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply 1 failed");
    collect_fn.apply(&context, vec![val2.clone()], &mut accumulator).await.expect("Apply 2 failed");
    collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply 3 failed"); // Duplicate

    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");

    // Check final list (order doesn't matter, but count does)
    match final_result {
        VariableValue::List(list) => {
            assert_eq!(list.len(), 3);
            assert_eq!(list.iter().filter(|&v| v == &val1).count(), 2);
            assert_eq!(list.iter().filter(|&v| v == &val2).count(), 1);
        }
        _ => panic!("Expected VariableValue::List"),
    }
}

 #[tokio::test]
async fn test_collect_with_nulls() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let val1 = VariableValue::Integer(Integer::from(1));
    let val_null = VariableValue::Null;
    let val2 = VariableValue::String("world".to_string());

    collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply 1 failed");
    collect_fn.apply(&context, vec![val_null.clone()], &mut accumulator).await.expect("Apply null 1 failed"); // Null should be ignored
    collect_fn.apply(&context, vec![val2.clone()], &mut accumulator).await.expect("Apply 2 failed");
    collect_fn.apply(&context, vec![val_null.clone()], &mut accumulator).await.expect("Apply null 2 failed"); // Null should be ignored

    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::from_iter(vec![val1, val2]));
}

#[tokio::test]
async fn test_collect_empty() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::new());
}

 #[tokio::test]
async fn test_collect_only_nulls() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    collect_fn.apply(&context, vec![VariableValue::Null], &mut accumulator).await.expect("Apply null 1 failed");
    collect_fn.apply(&context, vec![VariableValue::Null], &mut accumulator).await.expect("Apply null 2 failed");

    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::new());
}

#[tokio::test]
async fn test_collect_apply_revert() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let val1 = VariableValue::Integer(Integer::from(1));
    let val2 = VariableValue::String("a".to_string());
    let val3 = VariableValue::Integer(Integer::from(1)); // Duplicate of val1

    // Apply
    collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply 1 failed");
    collect_fn.apply(&context, vec![val2.clone()], &mut accumulator).await.expect("Apply 2 failed");
    collect_fn.apply(&context, vec![val3.clone()], &mut accumulator).await.expect("Apply 3 failed"); // Apply duplicate

    // Snapshot after apply
    let after_apply = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot after apply failed");
    let expected_after_apply_set = HashSet::from_iter(vec![val1.clone(), val2.clone()]);
    // Need to handle potential list order differences if not using result_to_set
     match after_apply {
        VariableValue::List(list) => {
            let list_set: HashSet<_> = list.into_iter().collect();
            assert_eq!(list_set, expected_after_apply_set);
        }
        _ => panic!("Expected List after apply snapshot"),
    }


    // Check internal counts before revert
     match &accumulator {
        Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => {
             assert_eq!(counts.get(&val1), Some(&2));
             assert_eq!(counts.get(&val2), Some(&1));
        }
        _ => panic!("Incorrect accumulator type"),
    }

    // Revert one instance of val1
    let revert1_res = collect_fn.revert(&context, vec![val1.clone()], &mut accumulator).await.expect("Revert 1 failed");
    let expected_revert1_set = HashSet::from_iter(vec![val1.clone(), val2.clone()]); // Still contains val1 and val2
    assert_eq!(result_to_set(revert1_res), expected_revert1_set);

     // Check internal counts after first revert
     match &accumulator {
        Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => {
             assert_eq!(counts.get(&val1), Some(&1)); // Count decreased
             assert_eq!(counts.get(&val2), Some(&1));
        }
        _ => panic!("Incorrect accumulator type"),
    }

    // Revert val2
    let revert2_res = collect_fn.revert(&context, vec![val2.clone()], &mut accumulator).await.expect("Revert 2 failed");
    let expected_revert2_set = HashSet::from_iter(vec![val1.clone()]); // Only val1 left
    assert_eq!(result_to_set(revert2_res), expected_revert2_set);

    // Check internal counts after second revert
     match &accumulator {
        Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => {
             assert_eq!(counts.get(&val1), Some(&1));
             assert_eq!(counts.get(&val2), None); // val2 removed
        }
        _ => panic!("Incorrect accumulator type"),
    }

    // Revert final instance of val1
    let revert3_res = collect_fn.revert(&context, vec![val1.clone()], &mut accumulator).await.expect("Revert 3 failed");
    assert_eq!(result_to_set(revert3_res), HashSet::new()); // Empty now

     // Check internal counts after final revert
     match &accumulator {
        Accumulator::Value(ValueAccumulator::CollectCounts { counts }) => {
             assert!(counts.is_empty());
        }
        _ => panic!("Incorrect accumulator type"),
    }

    // Final snapshot check
    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Final snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::new());
}

 #[tokio::test]
async fn test_collect_revert_non_existent() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let val1 = VariableValue::Integer(Integer::from(1));
    let val_non_existent = VariableValue::String("nope".to_string());

    // Apply val1
    collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply failed");

    // Revert a value that wasn't added
    let revert_res = collect_fn.revert(&context, vec![val_non_existent], &mut accumulator).await.expect("Revert non-existent failed");
    // Should not error, and the state should remain unchanged
    assert_eq!(result_to_set(revert_res), HashSet::from_iter(vec![val1.clone()]));

    // Final snapshot check
    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::from_iter(vec![val1.clone()]));
}

#[tokio::test]
async fn test_collect_revert_null() {
    let collect_fn = Collect {};
    let context = create_test_context();
    let func_expr = dummy_func_expr();
    let grouping_keys = dummy_grouping_keys();
    let dummy_index = Arc::new(InMemoryResultIndex::new());

    let mut accumulator = collect_fn.initialize_accumulator(&context, &func_expr, &grouping_keys, dummy_index);

    let val1 = VariableValue::Integer(Integer::from(1));
    let val_null = VariableValue::Null;

    // Apply val1 and a null (which is ignored)
    collect_fn.apply(&context, vec![val1.clone()], &mut accumulator).await.expect("Apply 1 failed");
    collect_fn.apply(&context, vec![val_null.clone()], &mut accumulator).await.expect("Apply null failed");

    // Revert the null (should also be ignored and not change state)
    let revert_res = collect_fn.revert(&context, vec![val_null.clone()], &mut accumulator).await.expect("Revert null failed");
    assert_eq!(result_to_set(revert_res), HashSet::from_iter(vec![val1.clone()]));

    // Final snapshot check
    let final_result = collect_fn.snapshot(&context, vec![], &accumulator).await.expect("Snapshot failed");
    assert_eq!(result_to_set(final_result), HashSet::from_iter(vec![val1.clone()]));
}