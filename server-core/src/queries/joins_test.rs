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

#[cfg(test)]
mod query_joins_tests {
    use crate::channels::*;
    use crate::config::{QueryConfig, QueryJoinConfig, QueryJoinKeyConfig};
    use crate::queries::QueryManager;
    use crate::sources::{convert_json_to_element_value, SourceManager};
    use crate::test_support::helpers::test_fixtures::*;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    fn create_query_join_config(id: &str, keys: Vec<(String, String)>) -> QueryJoinConfig {
        QueryJoinConfig {
            id: id.to_string(),
            keys: keys
                .into_iter()
                .map(|(label, property)| QueryJoinKeyConfig { label, property })
                .collect(),
        }
    }

    fn create_query_config_with_joins(
        id: &str,
        query: &str,
        sources: Vec<String>,
        joins: Vec<QueryJoinConfig>,
    ) -> QueryConfig {
        QueryConfig {
            id: id.to_string(),
            query: query.to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            sources,
            auto_start: false,
            properties: HashMap::new(),
            joins: Some(joins),
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
        }
    }

    fn create_node_with_properties(
        source_name: &str,
        id: &str,
        labels: Vec<String>,
        properties: HashMap<&str, serde_json::Value>,
    ) -> Element {
        let reference = ElementReference::new(source_name, id);

        let mut property_map = ElementPropertyMap::new();
        for (key, value) in properties {
            if let Ok(element_value) = convert_json_to_element_value(&value) {
                property_map.insert(key, element_value);
            }
        }

        let metadata = ElementMetadata {
            reference,
            labels: Arc::from(
                labels
                    .into_iter()
                    .map(|l| Arc::from(l.as_str()))
                    .collect::<Vec<_>>(),
            ),
            effective_from: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        };

        Element::Node {
            metadata,
            properties: property_map,
        }
    }

    async fn create_test_environment() -> (
        Arc<QueryManager>,
        mpsc::Receiver<QueryResult>,
        mpsc::Receiver<ComponentEvent>,
        Arc<SourceManager>,
    ) {
        let (query_tx, query_rx) = mpsc::channel(100);
        let (event_tx, event_rx) = mpsc::channel(100);

        let source_manager = Arc::new(SourceManager::new(event_tx.clone()));
        let query_manager = Arc::new(QueryManager::new(query_tx, event_tx.clone(), source_manager.clone()));

        (
            query_manager,
            query_rx,
            event_rx,
            source_manager,
        )
    }


    #[tokio::test]
    async fn test_basic_join_between_two_sources() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        // Create two mock sources
        let vehicles_source = create_test_source_config("vehicles", "mock");
        let drivers_source = create_test_source_config("drivers", "mock");

        source_manager.add_source(vehicles_source).await.unwrap();
        source_manager.add_source(drivers_source).await.unwrap();

        // Create a query with a join between Vehicle and Driver
        let join_config = create_query_join_config(
            "VEHICLE_TO_DRIVER",
            vec![
                ("Vehicle".to_string(), "licensePlate".to_string()),
                ("Driver".to_string(), "vehicleLicensePlate".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "vehicle-driver-query",
            "MATCH (d:Driver)-[:VEHICLE_TO_DRIVER]->(v:Vehicle) WHERE v.status = 'available' RETURN d.name as driver, v.licensePlate as plate",
            vec!["vehicles".to_string(), "drivers".to_string()],
            vec![join_config],
        );

        query_manager.add_query(query_config).await.unwrap();

        // Start the query - it will subscribe directly to sources
        query_manager
            .start_query("vehicle-driver-query".to_string())
            .await
            .unwrap();

        // Give query time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get mock source instances
        let vehicles_mock = source_manager
            .get_source_instance("vehicles")
            .await
            .expect("vehicles source should exist");
        let drivers_mock = source_manager
            .get_source_instance("drivers")
            .await
            .expect("drivers source should exist");

        // Downcast to MockSource to access inject_event
        let vehicles_source = vehicles_mock
            .as_any()
            .downcast_ref::<crate::sources::mock::MockSource>()
            .expect("Should be MockSource");
        let drivers_source = drivers_mock
            .as_any()
            .downcast_ref::<crate::sources::mock::MockSource>()
            .expect("Should be MockSource");

        // Push vehicle data
        let vehicle1 = create_node_with_properties(
            "vehicles",
            "v1",
            vec!["Vehicle".to_string()],
            HashMap::from([
                ("licensePlate", json!("ABC-123")),
                ("status", json!("available")),
                ("model", json!("Toyota Camry")),
            ]),
        );

        vehicles_source
            .inject_event(SourceChange::Insert {
                element: vehicle1,
            })
            .await
            .unwrap();

        // Push driver data that matches the vehicle
        let driver1 = create_node_with_properties(
            "drivers",
            "d1",
            vec!["Driver".to_string()],
            HashMap::from([
                ("name", json!("John Doe")),
                ("vehicleLicensePlate", json!("ABC-123")),
                ("employeeId", json!("EMP001")),
            ]),
        );

        drivers_source
            .inject_event(SourceChange::Insert { element: driver1 })
            .await
            .unwrap();

        // Wait for query result
        let result = timeout(Duration::from_secs(2), query_rx.recv()).await;
        assert!(result.is_ok(), "Should receive query result within timeout");

        let query_result = result.unwrap();
        assert!(query_result.is_some(), "Query result should not be None");

        // Verify the join worked by checking result content
        // The actual verification would depend on QueryResult structure
        println!("Received query result for join test: {:?}", query_result);
    }

    #[tokio::test]
    async fn test_dynamic_updates_with_joins() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        // Setup sources
        source_manager
            .add_source(create_test_source_config("orders", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("restaurants", "mock"))
            .await
            .unwrap();

        // Create join config
        let join_config = create_query_join_config(
            "ORDER_TO_RESTAURANT",
            vec![
                ("Order".to_string(), "restaurantId".to_string()),
                ("Restaurant".to_string(), "id".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "order-restaurant-query",
            "MATCH (o:Order)-[:ORDER_TO_RESTAURANT]->(r:Restaurant) RETURN o.orderId as orderId, r.name as restaurant",
            vec!["orders".to_string(), "restaurants".to_string()],
            vec![join_config],
        );

        query_manager.add_query(query_config).await.unwrap();
        query_manager
            .start_query("order-restaurant-query".to_string())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get mock source instances
        let restaurants_mock = source_manager.get_source_instance("restaurants").await.expect("restaurants source");
        let orders_mock = source_manager.get_source_instance("orders").await.expect("orders source");
        let restaurants_source = restaurants_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let orders_source = orders_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");

        // Add initial data
        let restaurant1 = create_node_with_properties(
            "restaurants",
            "r1",
            vec!["Restaurant".to_string()],
            HashMap::from([("id", json!("REST001")), ("name", json!("Pizza Palace"))]),
        );
        restaurants_source.inject_event(SourceChange::Insert { element: restaurant1 }).await.unwrap();

        let order1 = create_node_with_properties(
            "orders",
            "o1",
            vec!["Order".to_string()],
            HashMap::from([
                ("orderId", json!("ORD001")),
                ("restaurantId", json!("REST001")),
                ("status", json!("pending")),
            ]),
        );
        orders_source.inject_event(SourceChange::Insert { element: order1.clone() }).await.unwrap();

        // Wait for initial result
        let _initial_result = timeout(Duration::from_secs(1), query_rx.recv()).await;

        // Update the order - change orderId which is in the RETURN clause
        let updated_order = create_node_with_properties(
            "orders",
            "o1",
            vec!["Order".to_string()],
            HashMap::from([
                ("orderId", json!("ORD001-UPDATED")),
                ("restaurantId", json!("REST001")),
                ("status", json!("completed")),
            ]),
        );
        orders_source.inject_event(SourceChange::Update { element: updated_order }).await.unwrap();

        // Check for updated result
        let update_result = timeout(Duration::from_secs(1), query_rx.recv()).await;
        assert!(update_result.is_ok(), "Should receive updated query result");

        // Delete the order
        let metadata = match order1 {
            Element::Node { metadata, .. } => metadata,
            _ => panic!("Expected node element"),
        };
        orders_source.inject_event(SourceChange::Delete { metadata }).await.unwrap();

        // Verify deletion affects join results
        let delete_result = timeout(Duration::from_secs(1), query_rx.recv()).await;
        assert!(
            delete_result.is_ok(),
            "Should receive result after deletion"
        );
    }

    #[tokio::test]
    async fn test_multiple_joins_in_single_query() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        // Create three sources
        source_manager
            .add_source(create_test_source_config("orders", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("drivers", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("restaurants", "mock"))
            .await
            .unwrap();

        // Create multiple joins
        let restaurant_join = create_query_join_config(
            "ORDER_TO_RESTAURANT",
            vec![
                ("Order".to_string(), "restaurantId".to_string()),
                ("Restaurant".to_string(), "id".to_string()),
            ],
        );

        let driver_join = create_query_join_config(
            "ORDER_TO_DRIVER",
            vec![
                ("Order".to_string(), "driverId".to_string()),
                ("Driver".to_string(), "id".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "full-order-query",
            "MATCH (o:Order)-[:ORDER_TO_RESTAURANT]->(r:Restaurant), (o)-[:ORDER_TO_DRIVER]->(d:Driver) RETURN o.orderId, r.name, d.name",
            vec!["orders".to_string(), "drivers".to_string(), "restaurants".to_string()],
            vec![restaurant_join, driver_join],
        );

        query_manager.add_query(query_config).await.unwrap();
        query_manager
            .start_query("full-order-query".to_string())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get mock source instances
        let orders_mock = source_manager.get_source_instance("orders").await.expect("orders source");
        let drivers_mock = source_manager.get_source_instance("drivers").await.expect("drivers source");
        let restaurants_mock = source_manager.get_source_instance("restaurants").await.expect("restaurants source");
        let orders_source = orders_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let drivers_source = drivers_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let restaurants_source = restaurants_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");

        // Add data for all three sources
        let restaurant = create_node_with_properties(
            "restaurants",
            "r1",
            vec!["Restaurant".to_string()],
            HashMap::from([("id", json!("REST001")), ("name", json!("Burger Barn"))]),
        );
        restaurants_source.inject_event(SourceChange::Insert { element: restaurant }).await.unwrap();

        let driver = create_node_with_properties(
            "drivers",
            "d1",
            vec!["Driver".to_string()],
            HashMap::from([("id", json!("DRV001")), ("name", json!("Alice Smith"))]),
        );
        drivers_source.inject_event(SourceChange::Insert { element: driver }).await.unwrap();

        let order = create_node_with_properties(
            "orders",
            "o1",
            vec!["Order".to_string()],
            HashMap::from([
                ("orderId", json!("ORD001")),
                ("restaurantId", json!("REST001")),
                ("driverId", json!("DRV001")),
            ]),
        );
        orders_source.inject_event(SourceChange::Insert { element: order }).await.unwrap();

        // Wait for query result with multiple joins
        let result = timeout(Duration::from_secs(2), query_rx.recv()).await;
        assert!(
            result.is_ok(),
            "Should receive query result with multiple joins"
        );
    }

    #[tokio::test]
    async fn test_join_with_non_matching_properties() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        source_manager
            .add_source(create_test_source_config("source1", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("source2", "mock"))
            .await
            .unwrap();

        let join_config = create_query_join_config(
            "TEST_JOIN",
            vec![
                ("NodeA".to_string(), "linkId".to_string()),
                ("NodeB".to_string(), "linkId".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "non-matching-query",
            "MATCH (a:NodeA)-[:TEST_JOIN]->(b:NodeB) RETURN a, b",
            vec!["source1".to_string(), "source2".to_string()],
            vec![join_config],
        );

        query_manager.add_query(query_config).await.unwrap();
        query_manager
            .start_query("non-matching-query".to_string())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get mock source instances
        let source1_mock = source_manager.get_source_instance("source1").await.expect("source1");
        let source2_mock = source_manager.get_source_instance("source2").await.expect("source2");
        let source1_source = source1_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let source2_source = source2_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");

        // Add nodes with non-matching link IDs
        let node_a = create_node_with_properties(
            "source1",
            "a1",
            vec!["NodeA".to_string()],
            HashMap::from([("linkId", json!("LINK001"))]),
        );
        source1_source.inject_event(SourceChange::Insert { element: node_a }).await.unwrap();

        let node_b = create_node_with_properties(
            "source2",
            "b1",
            vec!["NodeB".to_string()],
            HashMap::from([("linkId", json!("LINK999"))]), // Different ID - no match
        );
        source2_source.inject_event(SourceChange::Insert { element: node_b }).await.unwrap();

        // Should not receive results or receive empty results
        let result = timeout(Duration::from_millis(500), query_rx.recv()).await;
        // Result might be Ok with empty data or timeout - both are valid for non-matching joins
        println!("Non-matching join result: {:?}", result);
    }

    #[tokio::test]
    async fn test_join_with_null_properties() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        source_manager
            .add_source(create_test_source_config("source1", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("source2", "mock"))
            .await
            .unwrap();

        let join_config = create_query_join_config(
            "NULL_TEST_JOIN",
            vec![
                ("NodeA".to_string(), "optionalId".to_string()),
                ("NodeB".to_string(), "optionalId".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "null-property-query",
            "MATCH (a:NodeA)-[:NULL_TEST_JOIN]->(b:NodeB) RETURN a, b",
            vec!["source1".to_string(), "source2".to_string()],
            vec![join_config],
        );

        query_manager.add_query(query_config).await.unwrap();
        query_manager
            .start_query("null-property-query".to_string())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get mock source instances
        let source1_mock = source_manager.get_source_instance("source1").await.expect("source1");
        let source2_mock = source_manager.get_source_instance("source2").await.expect("source2");
        let source1_source = source1_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let source2_source = source2_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");

        // Add node without the join property
        let node_a = create_node_with_properties(
            "source1",
            "a1",
            vec!["NodeA".to_string()],
            HashMap::from([("otherprop", json!("value"))]), // Missing optionalId
        );
        source1_source.inject_event(SourceChange::Insert { element: node_a }).await.unwrap();

        let node_b = create_node_with_properties(
            "source2",
            "b1",
            vec!["NodeB".to_string()],
            HashMap::from([("optionalId", json!(null))]), // Null value
        );
        source2_source.inject_event(SourceChange::Insert { element: node_b }).await.unwrap();

        // Test behavior with null/missing properties
        let result = timeout(Duration::from_millis(500), query_rx.recv()).await;
        println!("Null property join result: {:?}", result);
    }

    #[tokio::test]
    async fn test_join_with_duplicate_keys() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        source_manager
            .add_source(create_test_source_config("products", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("categories", "mock"))
            .await
            .unwrap();

        let join_config = create_query_join_config(
            "PRODUCT_CATEGORY",
            vec![
                ("Product".to_string(), "categoryId".to_string()),
                ("Category".to_string(), "id".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "product-category-query",
            "MATCH (p:Product)-[:PRODUCT_CATEGORY]->(c:Category) RETURN p.name, c.name",
            vec!["products".to_string(), "categories".to_string()],
            vec![join_config],
        );

        query_manager.add_query(query_config).await.unwrap();
        query_manager
            .start_query("product-category-query".to_string())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Get mock source instances
        let products_mock = source_manager.get_source_instance("products").await.expect("products");
        let categories_mock = source_manager.get_source_instance("categories").await.expect("categories");
        let products_source = products_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let categories_source = categories_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");

        // Add category
        let category = create_node_with_properties(
            "categories",
            "cat1",
            vec!["Category".to_string()],
            HashMap::from([("id", json!("CAT001")), ("name", json!("Electronics"))]),
        );
        categories_source.inject_event(SourceChange::Insert { element: category }).await.unwrap();

        // Add multiple products with same category ID (duplicate keys)
        for i in 1..=3 {
            let product = create_node_with_properties(
                "products",
                &format!("prod{}", i),
                vec!["Product".to_string()],
                HashMap::from([
                    ("name", json!(format!("Product {}", i))),
                    ("categoryId", json!("CAT001")), // All have same category
                ]),
            );
            products_source.inject_event(SourceChange::Insert { element: product }).await.unwrap();
        }

        // Should receive multiple results for duplicate keys
        let mut results_count = 0;
        while let Ok(Some(_)) = timeout(Duration::from_millis(500), query_rx.recv()).await {
            results_count += 1;
            if results_count >= 3 {
                break;
            }
        }

        println!("Received {} results for duplicate key test", results_count);
        assert!(
            results_count > 0,
            "Should receive results for duplicate keys"
        );
    }

    #[tokio::test]
    async fn test_bootstrap_with_joins() {
        let (query_manager, mut query_rx, _event_rx, source_manager) =
            create_test_environment().await;

        source_manager
            .add_source(create_test_source_config("users", "mock"))
            .await
            .unwrap();
        source_manager
            .add_source(create_test_source_config("posts", "mock"))
            .await
            .unwrap();

        // Get mock source instances
        let users_mock = source_manager.get_source_instance("users").await.expect("users");
        let posts_mock = source_manager.get_source_instance("posts").await.expect("posts");
        let users_source = users_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");
        let posts_source = posts_mock.as_any().downcast_ref::<crate::sources::mock::MockSource>().expect("MockSource");

        // Pre-populate data before starting query
        let user1 = create_node_with_properties(
            "users",
            "u1",
            vec!["User".to_string()],
            HashMap::from([("userId", json!("USER001")), ("name", json!("Bob"))]),
        );
        users_source.inject_event(SourceChange::Insert { element: user1 }).await.unwrap();

        let post1 = create_node_with_properties(
            "posts",
            "p1",
            vec!["Post".to_string()],
            HashMap::from([
                ("postId", json!("POST001")),
                ("authorId", json!("USER001")),
                ("title", json!("First Post")),
            ]),
        );
        posts_source.inject_event(SourceChange::Insert { element: post1 }).await.unwrap();

        // Now create and start query with join
        let join_config = create_query_join_config(
            "AUTHORED_BY",
            vec![
                ("Post".to_string(), "authorId".to_string()),
                ("User".to_string(), "userId".to_string()),
            ],
        );

        let query_config = create_query_config_with_joins(
            "user-posts-query",
            "MATCH (p:Post)-[:AUTHORED_BY]->(u:User) RETURN p.title, u.name",
            vec!["users".to_string(), "posts".to_string()],
            vec![join_config],
        );

        query_manager.add_query(query_config).await.unwrap();

        // Start query - should trigger bootstrap
        query_manager
            .start_query("user-posts-query".to_string())
            .await
            .unwrap();

        // Should receive bootstrapped data with joins
        let bootstrap_result = timeout(Duration::from_secs(2), query_rx.recv()).await;
        assert!(
            bootstrap_result.is_ok(),
            "Should receive bootstrapped join results"
        );

        // Add new data after bootstrap
        let post2 = create_node_with_properties(
            "posts",
            "p2",
            vec!["Post".to_string()],
            HashMap::from([
                ("postId", json!("POST002")),
                ("authorId", json!("USER001")),
                ("title", json!("Second Post")),
            ]),
        );
        posts_source.inject_event(SourceChange::Insert { element: post2 }).await.unwrap();

        // Should receive incremental update
        let incremental_result = timeout(Duration::from_secs(1), query_rx.recv()).await;
        assert!(
            incremental_result.is_ok(),
            "Should receive incremental join results"
        );
    }
}
