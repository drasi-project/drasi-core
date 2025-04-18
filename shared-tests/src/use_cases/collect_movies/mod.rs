#![allow(clippy::unwrap_used)]
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

use std::sync::Arc;
use std::collections::{BTreeMap, HashSet};

use serde_json::json;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

use crate::QueryTestConfig;

mod queries;

/// Macro to create a BTreeMap with string keys and VariableValue values
macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

/// Test data factory functions
#[derive(Debug)]
struct TestDataFactory;

impl TestDataFactory {
    /// Create a Person node SourceChange
    fn person(id: &str, name: &str) -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", id),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({ "name": name })),
            },
        }
    }

    /// Create a Movie node SourceChange
    fn movie(id: &str, title: &str, genre: &str) -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", id),
                    labels: Arc::new([Arc::from("Movie")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({ "title": title, "genre": genre })),
            },
        }
    }

    /// Create a LIKES relationship SourceChange
    fn likes(rel_id: &str, person_id: &str, movie_id: &str) -> SourceChange {
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", rel_id),
                    labels: Arc::new([Arc::from("LIKES")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(), // No properties on LIKES
                in_node: ElementReference::new("test", person_id),
                out_node: ElementReference::new("test", movie_id),
            },
        }
    }
    
    /// Get element ID from a SourceChange for logging
    fn get_element_id(change: &SourceChange) -> Arc<str> {
        match change {
            SourceChange::Insert { element } |
            SourceChange::Update { element } => element.get_metadata().reference.element_id.clone(),
            SourceChange::Delete { metadata } => metadata.reference.element_id.clone(),
            SourceChange::Future { future_ref } => future_ref.element_ref.element_id.clone(),
        }
    }
}

/// Test assertions for the collect function
struct CollectAssertions;

impl CollectAssertions {
    /// Assert an aggregation result matches expected values
    fn assert_aggregation_result(
        result: &[QueryPartEvaluationContext],
        expected_grouping_keys: Vec<VariableValue>,
        expected_before: Option<BTreeMap<Arc<str>, VariableValue>>,
        expected_after: Option<BTreeMap<Arc<str>, VariableValue>>,
        default_before: bool,
        default_after: bool,
    ) {
        assert_eq!(result.len(), 1, "Expected exactly one result context");
        
        match &result[0] {
            QueryPartEvaluationContext::Aggregation {
                grouping_keys,
                before,
                after,
                default_before: actual_default_before,
                default_after: actual_default_after,
            } => {
                // Compare grouping keys
                assert_eq!(grouping_keys, &expected_grouping_keys, "Grouping keys mismatch");
                assert_eq!(actual_default_before, &default_before, "Default before mismatch");
                assert_eq!(actual_default_after, &default_after, "Default after mismatch");

                // Compare 'before' state
                Self::verify_before_state(before, &expected_before, default_before);
                
                // Compare 'after' state
                Self::verify_after_state(after, &expected_after, *actual_default_after);
            }
            other => panic!("Expected Aggregation context, got {:?}", other),
        }
    }
    
    /// Verify the 'before' state matches expectations
    fn verify_before_state(
        actual: &Option<BTreeMap<Box<str>, VariableValue>>,
        expected: &Option<BTreeMap<Arc<str>, VariableValue>>,
        default_before: bool,
    ) {
        match (actual, expected) {
            (Some(actual_state), Some(expected_state)) => {
                Self::assert_maps_equal_list_as_set(actual_state, expected_state, "before");
            }
            (Some(_), None) if default_before => {
                // Skip validation for default before state when no expectation is provided
            }
            (None, None) => {
                // Both are None, which is valid
            }
            _ => panic!(
                "Before state mismatch: is_some: {}, expected_is_some: {}", 
                actual.is_some(), 
                expected.is_some()
            ),
        }
    }
    
    /// Verify the 'after' state matches expectations
    fn verify_after_state(
        actual: &BTreeMap<Box<str>, VariableValue>,
        expected: &Option<BTreeMap<Arc<str>, VariableValue>>,
        default_after: bool,
    ) {
        match expected {
            Some(expected_state) => {
                // Always validate when an expectation is provided, even for default states
                Self::assert_maps_equal_list_as_set(actual, expected_state, "after");
            }
            None => {
                // Only require expected_after to be None when default_after is true
                if !default_after {
                    panic!("default_after is false, but expected_after is None");
                }
                // Otherwise, skip validation for default after state
            }
        }
    }

    /// Assert equality between BTreeMaps, treating Lists as Sets for order independence
    fn assert_maps_equal_list_as_set(
        actual: &BTreeMap<Box<str>, VariableValue>,
        expected: &BTreeMap<Arc<str>, VariableValue>,
        context_name: &str,
    ) {
        assert_eq!(
            actual.len(), 
            expected.len(), 
            "Map length mismatch in {}", 
            context_name
        );
        
        for ((actual_key, actual_value), (expected_key, expected_value)) in actual.iter().zip(expected.iter()) {
            assert_eq!(
                actual_key.as_ref(), 
                expected_key.as_ref(), 
                "Map key mismatch in {}", 
                context_name
            );
            
            match (actual_value, expected_value) {
                (VariableValue::List(actual_list), VariableValue::List(expected_list)) => {
                    let actual_set: HashSet<_> = actual_list.iter().cloned().collect();
                    let expected_set: HashSet<_> = expected_list.iter().cloned().collect();
                    assert_eq!(
                        actual_set, 
                        expected_set, 
                        "List content mismatch (order ignored) for key '{}' in {}", 
                        actual_key.as_ref(), 
                        context_name
                    );
                }
                (actual_val, expected_val) => assert_eq!(
                    actual_val, 
                    expected_val, 
                    "Value mismatch for key '{}' in {}", 
                    actual_key.as_ref(), 
                    context_name
                ),
            }
        }
    }
    
    /// Find an aggregation context for a specific person in a list of contexts
    fn find_person_context<'a>(
        contexts: &'a [QueryPartEvaluationContext],
        person_name: &str
    ) -> Option<&'a QueryPartEvaluationContext> {
        contexts.iter().find(|ctx| {
            if let QueryPartEvaluationContext::Aggregation { after, .. } = ctx {
                after.get("Person").map_or(false, |p| p == &VariableValue::from(person_name))
            } else {
                false
            }
        })
    }
}

/// End-to-end test for the collect function with a movie database scenario
#[allow(clippy::print_stdout)]
pub async fn collect_movies(config: &(impl QueryTestConfig + Send)) {
    println!("--- Starting collect_movies test ---");

    // Initialize query with configuration
    let collect_query = {
        let mut builder = QueryBuilder::new(queries::collect_movies_query());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // --- Create Nodes ---
    let nodes = vec![
        TestDataFactory::person("alice", "Alice"),
        TestDataFactory::person("bob", "Bob"),
        TestDataFactory::person("carol", "Carol"),
        TestDataFactory::movie("hangover", "The Hangover", "Comedy"),
        TestDataFactory::movie("superbad", "Superbad", "Comedy"),
        TestDataFactory::movie("inception", "Inception", "Sci-Fi"),
        TestDataFactory::movie("stepbrothers", "Step Brothers", "Comedy"),
    ];
    
    // Process nodes
    for node_change in nodes {
        let element_id = TestDataFactory::get_element_id(&node_change);
        println!("Processing node: {}", element_id);
        
        let result = collect_query.process_source_change(node_change).await.unwrap();
        assert!(
            result.is_empty(), 
            "Node creation produced unexpected output: {:?}", 
            result
        );
    }
    println!("--- Nodes Created ---");

    // --- Test Case 1: Alice LIKES Hangover (Comedy) ---
    println!("Test 1: Alice -> Hangover");
    let alice_likes_hangover = TestDataFactory::likes("rel1", "alice", "hangover");
    let result1 = collect_query.process_source_change(alice_likes_hangover).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result1,
        vec![VariableValue::from("Person")],
        None, // Before state is None (new group)
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![VariableValue::from("The Hangover")])
        )),
        true,  // default_before 
        false  // default_after
    );

    // --- Test Case 2: Alice LIKES Superbad (Comedy) ---
    println!("Test 2: Alice -> Superbad");
    let alice_likes_superbad = TestDataFactory::likes("rel2", "alice", "superbad");
    let result2 = collect_query.process_source_change(alice_likes_superbad).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result2,
        vec![VariableValue::from("Person")],
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![VariableValue::from("The Hangover")])
        )),
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("The Hangover"),
                VariableValue::from("Superbad")
            ])
        )),
        true,
        false 
    );

    // --- Test Case 3: Alice LIKES Inception (Sci-Fi - Should be filtered) ---
    println!("Test 3: Alice -> Inception (Sci-Fi)");
    let alice_likes_inception = TestDataFactory::likes("rel3", "alice", "inception");
    let result3 = collect_query.process_source_change(alice_likes_inception).await.unwrap();
    
    assert!(
        result3.is_empty(), 
        "Sci-Fi movie produced unexpected output: {:?}", 
        result3
    );

    // --- Test Case 4: Bob LIKES Step Brothers (Comedy) ---
    println!("Test 4: Bob -> Step Brothers");
    let bob_likes_stepbrothers = TestDataFactory::likes("rel4", "bob", "stepbrothers");
    let result4 = collect_query.process_source_change(bob_likes_stepbrothers).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result4,
        vec![VariableValue::from("Person")],
        None, // Before state is None (new group)
        Some(variablemap!(
            "Person" => VariableValue::from("Bob"),
            "Comedies_Liked" => VariableValue::List(vec![VariableValue::from("Step Brothers")])
        )),
        true,
        false
    );

    // --- Test Case 5: Carol LIKES Inception (Sci-Fi - Should be filtered) ---
    println!("Test 5: Carol -> Inception (Sci-Fi)");
    let carol_likes_inception = TestDataFactory::likes("rel5", "carol", "inception");
    let result5 = collect_query.process_source_change(carol_likes_inception).await.unwrap();
    
    assert!(
        result5.is_empty(), 
        "Sci-Fi movie produced unexpected output: {:?}", 
        result5
    );

    // --- Test Case 6: Update Inception from Sci-Fi to Comedy ---
    println!("Test 6: Update Inception genre -> Comedy");
    let update_inception = SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "inception"),
                labels: Arc::new([Arc::from("Movie")]),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({ 
                "title": "Inception", 
                "genre": "Comedy" 
            })),
        },
    };
    
    let result6 = collect_query.process_source_change(update_inception).await.unwrap();
    
    // This change affects two rows, so we need to check each one individually
    assert_eq!(result6.len(), 2, "Expected two result contexts");
    
    // Find Carol's result (new row)
    let carol_result = CollectAssertions::find_person_context(&result6, "Carol")
        .expect("Carol's result not found");
    
    // Find Alice's result (updated row)
    let alice_result = CollectAssertions::find_person_context(&result6, "Alice")
        .expect("Alice's result not found");
    
    // Check Carol's row (new)
    match carol_result {
        QueryPartEvaluationContext::Aggregation { before, after, .. } => {
            assert!(before.is_none(), "Carol's before state should be None");
            assert_eq!(
                after.get("Comedies_Liked").unwrap(), 
                &VariableValue::List(vec![VariableValue::from("Inception")])
            );
        },
        _ => panic!("Expected Aggregation context for Carol"),
    }
    
    // Check Alice's row (updated)
    match alice_result {
        QueryPartEvaluationContext::Aggregation { after, .. } => {
            // Verify that Alice's updated list contains Inception
            if let Some(comedies) = after.get("Comedies_Liked") {
                if let VariableValue::List(movies) = comedies {
                    assert!(
                        movies.contains(&VariableValue::from("Inception")), 
                        "Inception should be in Alice's updated list"
                    );
                    
                    // Also verify that other movies are still there
                    assert!(movies.contains(&VariableValue::from("The Hangover")));
                    assert!(movies.contains(&VariableValue::from("Superbad")));
                } else {
                    panic!("Expected list for Comedies_Liked");
                }
            } else {
                panic!("Missing Comedies_Liked in after state");
            }
        },
        _ => panic!("Expected Aggregation context for Alice"),
    }

    // --- Test Case 7: Remove a relationship that shrinks a list but keeps the row ---
    println!("Test 7: Delete Alice -> Superbad relationship");
    let delete_alice_superbad = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "rel2"),
            labels: Arc::new([Arc::from("LIKES")]),
            effective_from: 0,
        },
    };
    
    let result7 = collect_query.process_source_change(delete_alice_superbad).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result7,
        vec![VariableValue::from("Person")],
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Superbad"),
                VariableValue::from("The Hangover"),
                VariableValue::from("Inception")
            ])
        )),
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("The Hangover"),
                VariableValue::from("Inception")
            ])
        )),
        false,
        true
    );

    // --- Test Case 8: Remove a relationship that changes but doesn't empty a list ---
    println!("Test 8: Delete Alice -> Hangover relationship");
    let delete_alice_hangover = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "rel1"),
            labels: Arc::new([Arc::from("LIKES")]),
            effective_from: 0,
        },
    };
    
    let result8 = collect_query.process_source_change(delete_alice_hangover).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result8,
        vec![VariableValue::from("Person")],
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("The Hangover"),
                VariableValue::from("Inception")
            ])
        )),
        Some(variablemap!(
            "Person" => VariableValue::from("Alice"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Inception")
            ])
        )),
        false,
        true
    );

    // --- Test Case 9: Delete whole movie node and verify cascading effects ---
    // First add a new relationship for Bob to setup the test
    println!("Test 9: Setup - Bob -> Hangover");
    let bob_likes_hangover = TestDataFactory::likes("rel9", "bob", "hangover");
    collect_query.process_source_change(bob_likes_hangover).await.unwrap();
    
    println!("Test 9: Delete Hangover movie node (cascades to relationships)");
    let delete_hangover = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "hangover"),
            labels: Arc::new([Arc::from("Movie")]),
            effective_from: 0,
        },
    };
    
    let result9 = collect_query.process_source_change(delete_hangover).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result9,
        vec![VariableValue::from("Person")],
        Some(variablemap!(
            "Person" => VariableValue::from("Bob"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Step Brothers"),
                VariableValue::from("The Hangover")
            ])
        )),
        Some(variablemap!(
            "Person" => VariableValue::from("Bob"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Step Brothers")
            ])
        )),
        false,
        true
    );

    // --- Test Case 10: Update a visible movie to make it invisible ---
    println!("Test 10: Update Step Brothers genre -> Drama (becomes invisible)");
    let update_stepbrothers = SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "stepbrothers"),
                labels: Arc::new([Arc::from("Movie")]),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({ 
                "title": "Step Brothers", 
                "genre": "Drama" 
            })),
        },
    };
    
    let result10 = collect_query.process_source_change(update_stepbrothers).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result10,
        vec![VariableValue::from("Person")],
        Some(variablemap!(
            "Person" => VariableValue::from("Bob"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Step Brothers")
            ])
        )),
        Some(variablemap!(
            "Person" => VariableValue::from("Bob"),
            "Comedies_Liked" => VariableValue::List(vec![])
        )),
        false,
        false
    );

    // --- Test Case 11: Test duplicate LIKES edges ---
    println!("Test 11: Add duplicate LIKES edge: Carol -> Inception");
    let carol_likes_inception_again = TestDataFactory::likes("rel11", "carol", "inception");
    let result11 = collect_query.process_source_change(carol_likes_inception_again).await.unwrap();
    
    CollectAssertions::assert_aggregation_result(
        &result11,
        vec![VariableValue::from("Person")],
        Some(variablemap!(
            "Person" => VariableValue::from("Carol"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Inception")
            ])
        )),
        Some(variablemap!(
            "Person" => VariableValue::from("Carol"),
            "Comedies_Liked" => VariableValue::List(vec![
                VariableValue::from("Inception"),
                VariableValue::from("Inception")
            ])
        )),
        true,
        false
    );

    println!("--- collect_movies test finished successfully ---");
}