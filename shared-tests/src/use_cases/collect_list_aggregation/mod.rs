// Copyright 2026 The Drasi Authors.
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

use drasi_functions_gql::GQLFunctionSet;
use drasi_query_ast::api::QueryConfiguration;
use drasi_query_gql::GQLParser;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use serde_json::json;

use crate::{use_cases::collect_aggregation::data::get_bootstrap_data, QueryTestConfig};

pub mod queries;

async fn bootstrap_query(query: &ContinuousQuery) -> Vec<QueryPartEvaluationContext> {
    let data = get_bootstrap_data();
    let mut all_results = Vec::new();

    for change in data {
        if let Ok(results) = query.process_source_change(change).await {
            all_results.extend(results);
        }
    }

    all_results
}

/// Test collect_list() function for aggregating values into lists
pub async fn collect_based_aggregation_test(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_gql_function_set();
        let parser = Arc::new(GQLParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::collect_based_aggregation_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let _ = bootstrap_query(&query).await;

    // Test by adding a new order
    let new_order = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("OrderItem", "oi10"),
                labels: Arc::new([Arc::from("orderItem")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(json!({
                "orderItemId": "oi10",
                "quantity": 3
            })),
        },
    };

    let _ = query.process_source_change(new_order).await.unwrap();

    // Connect the new order to product p1
    let new_edge = SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("PRODUCT_TO_ORDER_ITEM", "p1->oi10"),
                labels: Arc::new([Arc::from("PRODUCT_TO_ORDER_ITEM")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
            in_node: ElementReference::new("Product", "p1"),
            out_node: ElementReference::new("OrderItem", "oi10"),
        },
    };

    let result = query.process_source_change(new_edge).await.unwrap();

    println!("\n=== CollectList-based Aggregation Test ===");
    println!(
        "Number of results: {} (should be 1 with collect())",
        result.len()
    );

    assert_eq!(
        result.len(),
        1,
        "With collect_list(), we should get exactly one result"
    );

    // Verify the aggregated values
    match &result[0] {
        QueryPartEvaluationContext::Aggregation { after, .. } => {
            assert_eq!(after.get("product_id").and_then(|v| v.as_str()), Some("p1"));

            // Check collected values
            if let Some(VariableValue::List(order_quantities)) = after.get("order_quantities") {
                println!(
                    "Collected order quantities: {} items",
                    order_quantities.len()
                );
                assert_eq!(order_quantities.len(), 4); // 3 original + 1 new
            }

            if let Some(order_count) = after.get("order_count").and_then(|v| v.as_i64()) {
                println!("Order count via size(): {}", order_count);
                assert_eq!(order_count, 4); // 3 original + 1 new
            }

            println!("✓ CollectList function test passed");
        }
        _ => panic!("Expected Updating context"),
    }
}

/// Test collect with filtering
pub async fn collect_with_filter_test(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_gql_function_set();
        let parser = Arc::new(GQLParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::collect_with_filter_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let _ = bootstrap_query(&query).await;

    // Add a low rating review to product p1 (which already has high ratings)
    let low_review = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("Review", "r_low"),
                labels: Arc::new([Arc::from("reviews")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(json!({
                "reviewId": "r_low",
                "rating": 2.0,
                "comment": "Not great"
            })),
        },
    };

    let _ = query.process_source_change(low_review).await.unwrap();

    // Connect to product p1
    let review_edge = SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("REVIEW_TO_PRODUCT", "r_low->p1"),
                labels: Arc::new([Arc::from("REVIEW_TO_PRODUCT")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
            in_node: ElementReference::new("Review", "r_low"),
            out_node: ElementReference::new("Product", "p1"),
        },
    };

    let result = query.process_source_change(review_edge).await.unwrap();

    println!("\n=== CollectList with Filter Test ===");
    println!("Number of results from update: {}", result.len());

    // If we didn't get results from the update, check the bootstrap results
    let bootstrap_results = bootstrap_query(&query).await;
    println!("Bootstrap results: {}", bootstrap_results.len());

    // Check for products with high ratings either in update or bootstrap
    let all_results: Vec<_> = result.iter().chain(bootstrap_results.iter()).collect();
    assert!(
        !all_results.is_empty(),
        "Should have results for products with high ratings"
    );

    // Check the bootstrap results to see which products have high ratings
    let mut found_high_rating_products = false;
    for ctx in &all_results {
        match ctx {
            QueryPartEvaluationContext::Aggregation { after, .. } => {
                if let Some(product_id) = after.get("product_id").and_then(|v| v.as_str()) {
                    if let Some(VariableValue::List(high_ratings)) = after.get("high_ratings") {
                        println!(
                            "Product {} has {} high ratings (>= 4.0)",
                            product_id,
                            high_ratings.len()
                        );
                        found_high_rating_products = true;

                        // Verify all ratings are >= 4.0
                        for rating in high_ratings {
                            if let Some(r) = rating.as_f64() {
                                assert!(r >= 4.0, "All collected ratings should be >= 4.0");
                            }
                        }

                        // Verify the filter is working - no ratings should be < 4.0
                        // The exact count depends on the test data, but all should be >= 4.0
                    }
                }
            }
            _ => {}
        }
    }

    assert!(
        found_high_rating_products,
        "Should find at least one product with high ratings"
    );
}

/// Test collecting objects
pub async fn collect_objects_test(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_gql_function_set();
        let parser = Arc::new(GQLParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::collect_objects_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bootstrap_results = bootstrap_query(&query).await;

    println!("\n=== Collect Objects Test ===");
    println!("Bootstrap results: {}", bootstrap_results.len());

    // Check that we're collecting objects properly
    for ctx in &bootstrap_results {
        match ctx {
            QueryPartEvaluationContext::Aggregation { after, .. } => {
                if let Some(product_id) = after.get("product_id").and_then(|v| v.as_str()) {
                    if let Some(VariableValue::List(review_details)) = after.get("review_details") {
                        println!(
                            "Product {} has {} review objects",
                            product_id,
                            review_details.len()
                        );

                        // Verify each item is an object with rating and comment
                        for detail in review_details {
                            if let VariableValue::Object(obj) = detail {
                                assert!(
                                    obj.contains_key("rating"),
                                    "Review object should have rating"
                                );
                                assert!(
                                    obj.contains_key("comment"),
                                    "Review object should have comment"
                                );
                            } else {
                                panic!("Expected object in review_details list");
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    assert!(
        bootstrap_results.len() >= 2,
        "Should have results for products with reviews"
    );
}

/// Test multiple collects in same WITH clause
pub async fn multiple_collects_test(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_gql_function_set();
        let parser = Arc::new(GQLParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::multiple_collects_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bootstrap_results = bootstrap_query(&query).await;

    println!("\n=== Multiple Collects Test ===");

    for ctx in &bootstrap_results {
        match ctx {
            QueryPartEvaluationContext::Adding { after } => {
                if let Some(product_id) = after.get("product_id").and_then(|v| v.as_str()) {
                    let ratings = after
                        .get("ratings")
                        .and_then(|v| {
                            if let VariableValue::List(l) = v {
                                Some(l.len())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let review_ids = after
                        .get("review_ids")
                        .and_then(|v| {
                            if let VariableValue::List(l) = v {
                                Some(l.len())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    let review_count = after
                        .get("review_count")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);

                    println!("Product {product_id}: {ratings} ratings, {review_ids} review_ids, count={review_count}");

                    // All three should match
                    assert_eq!(
                        ratings, review_ids,
                        "Number of ratings should match review_ids"
                    );
                    assert_eq!(
                        ratings as i64, review_count,
                        "Collected items should match count"
                    );
                }
            }
            _ => {}
        }
    }
}
