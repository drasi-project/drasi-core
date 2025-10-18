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

//! Tests for ApplicationSource

#[cfg(test)]
mod tests {
    use crate::sources::application::PropertyMapBuilder;
    use crate::test_support::helpers::create_test_application_source;

    #[tokio::test]
    async fn test_application_source_creation() {
        let (_source, handle) = create_test_application_source("test-source").await;

        assert_eq!(handle.source_id(), "test-source");
    }

    #[tokio::test]
    async fn test_send_node_insert() {
        let (_source, handle) = create_test_application_source("test-source").await;

        let props = PropertyMapBuilder::new()
            .with_string("name", "John")
            .with_integer("age", 30)
            .build();

        let result = handle
            .send_node_insert("node-1", vec!["Person"], props)
            .await;
        assert!(result.is_ok(), "Should send node insert successfully");
    }

    #[tokio::test]
    async fn test_send_node_update() {
        let (_source, handle) = create_test_application_source("test-source").await;

        let props = PropertyMapBuilder::new()
            .with_string("name", "Jane")
            .with_integer("age", 25)
            .build();

        let result = handle
            .send_node_update("node-1", vec!["Person"], props)
            .await;
        assert!(result.is_ok(), "Should send node update successfully");
    }

    #[tokio::test]
    async fn test_send_delete() {
        let (_source, handle) = create_test_application_source("test-source").await;

        let result = handle.send_delete("node-1", vec!["Person"]).await;
        assert!(result.is_ok(), "Should send delete successfully");
    }

    #[tokio::test]
    async fn test_send_relation_insert() {
        let (_source, handle) = create_test_application_source("test-source").await;

        let props = PropertyMapBuilder::new()
            .with_string("since", "2020")
            .build();

        let result = handle
            .send_relation_insert("rel-1", vec!["KNOWS"], props, "node-1", "node-2")
            .await;
        assert!(result.is_ok(), "Should send relation insert successfully");
    }

    #[tokio::test]
    async fn test_send_batch() {
        let (_source, handle) = create_test_application_source("test-source").await;

        // Create empty batch - no need for props
        let changes = vec![];
        let result = handle.send_batch(changes).await;
        assert!(result.is_ok(), "Should send empty batch successfully");
    }

    #[tokio::test]
    async fn test_property_map_builder_string() {
        let props = PropertyMapBuilder::new()
            .with_string("key", "value")
            .build();

        // Verify property exists
        assert!(props.get("key").is_some());
    }

    #[tokio::test]
    async fn test_property_map_builder_integer() {
        let props = PropertyMapBuilder::new().with_integer("age", 42).build();

        // Verify property exists
        assert!(props.get("age").is_some());
    }

    #[tokio::test]
    async fn test_property_map_builder_bool() {
        let props = PropertyMapBuilder::new().with_bool("active", true).build();

        // Verify property exists
        assert!(props.get("active").is_some());
    }

    #[tokio::test]
    async fn test_property_map_builder_float() {
        let props = PropertyMapBuilder::new().with_float("price", 19.99).build();

        // Verify property exists
        assert!(props.get("price").is_some());
    }

    #[tokio::test]
    async fn test_property_map_builder_multiple_types() {
        let props = PropertyMapBuilder::new()
            .with_string("name", "Product")
            .with_integer("quantity", 100)
            .with_bool("available", true)
            .with_float("price", 29.99)
            .build();

        // Verify all properties exist
        assert!(props.get("name").is_some());
        assert!(props.get("quantity").is_some());
        assert!(props.get("available").is_some());
        assert!(props.get("price").is_some());
    }

    #[tokio::test]
    async fn test_property_map_builder_empty() {
        let _props = PropertyMapBuilder::new().build();
        // Empty property map creates successfully
    }

    #[tokio::test]
    async fn test_property_map_builder_chaining() {
        let props = PropertyMapBuilder::new()
            .with_string("a", "1")
            .with_string("b", "2")
            .with_string("c", "3")
            .build();

        // Verify all chained properties exist
        assert!(props.get("a").is_some());
        assert!(props.get("b").is_some());
        assert!(props.get("c").is_some());
    }

    #[tokio::test]
    async fn test_multiple_node_inserts() {
        let (_source, handle) = create_test_application_source("test-source").await;

        for i in 1..=5 {
            let props = PropertyMapBuilder::new()
                .with_string("name", format!("Person-{}", i))
                .with_integer("id", i)
                .build();

            let result = handle
                .send_node_insert(format!("node-{}", i), vec!["Person"], props)
                .await;
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_send_with_multiple_labels() {
        let (_source, handle) = create_test_application_source("test-source").await;

        let props = PropertyMapBuilder::new()
            .with_string("name", "MultiLabel")
            .build();

        let result = handle
            .send_node_insert("node-1", vec!["Person", "User", "Admin"], props)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_property_map_overwrite() {
        let props = PropertyMapBuilder::new()
            .with_string("key", "value1")
            .with_string("key", "value2")
            .build();

        // Later value should overwrite - verify key exists
        assert!(props.get("key").is_some());
    }

    #[tokio::test]
    async fn test_handle_clone() {
        let (_source, handle) = create_test_application_source("test-source").await;

        let handle2 = handle.clone();
        assert_eq!(handle.source_id(), handle2.source_id());

        let props = PropertyMapBuilder::new()
            .with_string("name", "Test")
            .build();

        // Both handles should work
        assert!(handle
            .send_node_insert("node-1", vec!["Person"], props.clone())
            .await
            .is_ok());
        assert!(handle2
            .send_node_insert("node-2", vec!["Person"], props)
            .await
            .is_ok());
    }
}
