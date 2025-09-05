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

use crate::relabel::RelabelMiddlewareFactory;
use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{FutureElementRef, MiddlewareSetupError, SourceMiddlewareFactory},
    models::{
        Element, ElementMetadata, ElementReference, ElementTimestamp, ElementValue, SourceChange,
        SourceMiddlewareConfig,
    },
};
use serde_json::{json, Value};

// --- Test Helpers ---

fn create_mw_config(config_json: Value) -> SourceMiddlewareConfig {
    SourceMiddlewareConfig {
        name: "test_relabel".into(),
        kind: "relabel".into(),
        config: config_json
            .as_object()
            .expect("Config JSON must be an object")
            .clone(),
    }
}

fn create_node_insert_change(labels: Vec<&str>, props: Value) -> SourceChange {
    let labels: Vec<Arc<str>> = labels.into_iter().map(|s| Arc::from(s)).collect();
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "node1"),
                labels: Arc::from(labels),
                effective_from: 0,
            },
            properties: props.into(),
        },
    }
}

fn create_node_update_change(labels: Vec<&str>, props: Value) -> SourceChange {
    let labels: Vec<Arc<str>> = labels.into_iter().map(|s| Arc::from(s)).collect();
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "node1"),
                labels: Arc::from(labels),
                effective_from: 1,
            },
            properties: props.into(),
        },
    }
}

fn create_relation_insert_change(labels: Vec<&str>, props: Value) -> SourceChange {
    let labels: Vec<Arc<str>> = labels.into_iter().map(|s| Arc::from(s)).collect();
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "rel1"),
                labels: Arc::from(labels),
                effective_from: 0,
            },
            properties: props.into(),
            in_node: ElementReference::new("test_source", "node1"),
            out_node: ElementReference::new("test_source", "node2"),
        },
    }
}

fn create_delete_change(labels: Vec<&str>) -> SourceChange {
    let labels: Vec<Arc<str>> = labels.into_iter().map(|s| Arc::from(s)).collect();
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", "node1"),
            labels: Arc::from(labels),
            effective_from: 2,
        },
    }
}

fn create_future_change() -> SourceChange {
    SourceChange::Future {
        future_ref: FutureElementRef {
            element_ref: ElementReference::new("test_source", "node1"),
            original_time: 0 as ElementTimestamp,
            due_time: 100 as ElementTimestamp,
            group_signature: 0,
        },
    }
}

mod process {
    use super::*;

    #[tokio::test]
    async fn test_basic_label_relabeling() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "Person": "User",
                "Company": "Organization"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change =
            create_node_insert_change(vec!["Person", "Employee"], json!({"name": "John Doe"}));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert { element } => {
                let metadata = element.get_metadata();
                assert_eq!(metadata.labels.len(), 2);
                assert!(metadata.labels.contains(&Arc::from("User")));
                assert!(metadata.labels.contains(&Arc::from("Employee")));
                assert!(!metadata.labels.contains(&Arc::from("Person")));
            }
            _ => panic!("Expected Insert change"),
        }
    }

    #[tokio::test]
    async fn test_partial_label_relabeling() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "OldLabel": "NewLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change =
            create_node_insert_change(vec!["OldLabel", "UnchangedLabel"], json!({"data": "test"}));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert { element } => {
                let metadata = element.get_metadata();
                assert_eq!(metadata.labels.len(), 2);
                assert!(metadata.labels.contains(&Arc::from("NewLabel")));
                assert!(metadata.labels.contains(&Arc::from("UnchangedLabel")));
                assert!(!metadata.labels.contains(&Arc::from("OldLabel")));
            }
            _ => panic!("Expected Insert change"),
        }
    }

    #[tokio::test]
    async fn test_no_matching_labels() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "NonExistentLabel": "NewLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(
            vec!["ExistingLabel", "AnotherLabel"],
            json!({"data": "test"}),
        );

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert { element } => {
                let metadata = element.get_metadata();
                assert_eq!(metadata.labels.len(), 2);
                assert!(metadata.labels.contains(&Arc::from("ExistingLabel")));
                assert!(metadata.labels.contains(&Arc::from("AnotherLabel")));
            }
            _ => panic!("Expected Insert change"),
        }
    }

    #[tokio::test]
    async fn test_multiple_label_mappings() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "Person": "User",
                "Company": "Organization",
                "Employee": "Staff"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(
            vec!["Person", "Company", "Employee", "Manager"],
            json!({"data": "test"}),
        );

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert { element } => {
                let metadata = element.get_metadata();
                assert_eq!(metadata.labels.len(), 4);
                assert!(metadata.labels.contains(&Arc::from("User")));
                assert!(metadata.labels.contains(&Arc::from("Organization")));
                assert!(metadata.labels.contains(&Arc::from("Staff")));
                assert!(metadata.labels.contains(&Arc::from("Manager")));

                // Original labels should be replaced
                assert!(!metadata.labels.contains(&Arc::from("Person")));
                assert!(!metadata.labels.contains(&Arc::from("Company")));
                assert!(!metadata.labels.contains(&Arc::from("Employee")));
            }
            _ => panic!("Expected Insert change"),
        }
    }

    #[tokio::test]
    async fn test_update_change_relabeling() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "TempLabel": "FinalLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_update_change(vec!["TempLabel"], json!({"updated": true}));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Update { element } => {
                let metadata = element.get_metadata();
                assert_eq!(metadata.labels.len(), 1);
                assert!(metadata.labels.contains(&Arc::from("FinalLabel")));
                assert!(!metadata.labels.contains(&Arc::from("TempLabel")));
            }
            _ => panic!("Expected Update change"),
        }
    }

    #[tokio::test]
    async fn test_relation_relabeling() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "KNOWS": "CONNECTED_TO",
                "WORKS_FOR": "EMPLOYED_BY"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_relation_insert_change(
            vec!["KNOWS", "SOCIAL_LINK"],
            json!({"since": "2020-01-01"}),
        );

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert { element } => {
                let metadata = element.get_metadata();
                assert_eq!(metadata.labels.len(), 2);
                assert!(metadata.labels.contains(&Arc::from("CONNECTED_TO")));
                assert!(metadata.labels.contains(&Arc::from("SOCIAL_LINK")));
                assert!(!metadata.labels.contains(&Arc::from("KNOWS")));
            }
            _ => panic!("Expected Insert change with Relation"),
        }
    }

    #[tokio::test]
    async fn test_delete_change_relabeling() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "OldLabel": "NewLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_delete_change(vec!["OldLabel", "KeepLabel"]);

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Delete { metadata } => {
                assert_eq!(metadata.labels.len(), 2);
                assert!(metadata.labels.contains(&Arc::from("NewLabel")));
                assert!(metadata.labels.contains(&Arc::from("KeepLabel")));
                assert!(!metadata.labels.contains(&Arc::from("OldLabel")));
            }
            _ => panic!("Expected Delete change"),
        }
    }

    #[tokio::test]
    async fn test_future_change_passthrough() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "AnyLabel": "NewLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_future_change();

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], source_change);
    }

    #[tokio::test]
    async fn test_properties_preserved() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "TestLabel": "NewTestLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let properties = json!({
            "name": "Test Node",
            "value": 42,
            "active": true
        });

        let source_change = create_node_insert_change(vec!["TestLabel"], properties.clone());

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert { element } => {
                let props = element.get_properties();
                assert_eq!(
                    props.get("name"),
                    Some(&ElementValue::String("Test Node".into()))
                );
                assert_eq!(props.get("value"), Some(&ElementValue::Integer(42)));
                assert_eq!(props.get("active"), Some(&ElementValue::Bool(true)));
            }
            _ => panic!("Expected Insert change"),
        }
    }
}

mod factory {
    use super::*;

    #[test]
    fn test_create_relabel_middleware_success() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "Person": "User",
                "Company": "Organization"
            }
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_empty_label_mappings_fails() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {}
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("At least one label mapping must be specified"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_missing_label_mappings_fails() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            // Missing label_mappings field
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("missing field `labelMappings`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_invalid_config_format() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": "invalid_format" // Should be an object, not string
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("Invalid configuration"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_factory_name() {
        let factory = RelabelMiddlewareFactory::new();
        assert_eq!(factory.name(), "relabel");
    }

    #[test]
    fn test_factory_default() {
        let factory = RelabelMiddlewareFactory::default();
        let config = json!({
            "labelMappings": {
                "Test": "NewTest"
            }
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_single_label_mapping() {
        let factory = RelabelMiddlewareFactory::new();
        let config = json!({
            "labelMappings": {
                "OnlyLabel": "SingleLabel"
            }
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_many_label_mappings() {
        let factory = RelabelMiddlewareFactory::new();
        let mut mappings = serde_json::Map::new();
        for i in 1..=100 {
            mappings.insert(format!("Label{}", i), json!(format!("NewLabel{}", i)));
        }
        let config = json!({
            "labelMappings": mappings
        });
        let mw_config = create_mw_config(config);
        let result = factory.create(&mw_config);
        assert!(result.is_ok());
    }
}
