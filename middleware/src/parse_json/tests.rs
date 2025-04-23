use std::sync::Arc;

use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{MiddlewareSetupError, SourceMiddlewareFactory},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
        SourceMiddlewareConfig,
    },
};
use ordered_float::OrderedFloat;
use serde_json::{json, Map, Value};

use crate::parse_json::ParseJsonFactory;

// --- Test Helpers ---

fn create_mw_config(config_json: Value) -> SourceMiddlewareConfig {
    let config_map = match config_json {
        Value::Object(map) => map,
        _ => panic!("Expected JSON object for config"),
    };
    SourceMiddlewareConfig {
        name: Arc::from("test_parse_json"),
        kind: "parse_json".into(),
        config: config_map,
    }
}

// Helper to create a basic node element with specific properties
fn create_node_element(props_map: ElementPropertyMap) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from("test_source"),
                element_id: Arc::from("node:1"),
            },
            labels: vec![Arc::from("TestNode")].into(),
            effective_from: 0,
        },
        properties: props_map,
    }
}

// Helper to create an ElementPropertyMap from serde_json::Value
fn create_prop_map(props_json: Value) -> ElementPropertyMap {
    let mut map = ElementPropertyMap::default();
    if let Value::Object(obj) = props_json {
        for (k, v) in obj {
            map.insert(&k, ElementValue::from(&v));
        }
    }
    map
}

fn create_node_insert_change(props: Value) -> SourceChange {
    SourceChange::Insert {
        element: create_node_element(create_prop_map(props)),
    }
}

fn create_node_update_change(props: Value) -> SourceChange {
    SourceChange::Update {
        element: create_node_element(create_prop_map(props)),
    }
}

fn create_node_delete_change() -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from("test_source"),
                element_id: Arc::from("node:1"),
            },
            labels: vec![Arc::from("TestNode")].into(),
            effective_from: 0,
        },
    }
}

fn get_props_from_change(change: &SourceChange) -> &ElementPropertyMap {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_properties()
        }
        SourceChange::Delete { .. } | SourceChange::Future { .. } => {
            panic!("Cannot get properties from Delete or Future change")
        }
    }
}

// Helper to assert that a property has an expected value
fn assert_property(props: &ElementPropertyMap, property: &str, expected: &ElementValue) {
    assert_eq!(
        props.get(property),
        Some(expected),
        "Property '{}' doesn't match expected value",
        property
    );
}

// --- Test Modules ---

mod process {
    use super::*;

    #[tokio::test]
    async fn test_parse_json_object_success_overwrite() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data_json",
        }));
        let middleware = factory
            .create(&config)
            .expect("Failed to create middleware");
        let index = InMemoryElementIndex::new();

        let input_change = create_node_insert_change(json!({
            "id": 1,
            "data_json": r#"{"key": "value", "num": 123}"#
        }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);

        let props = get_props_from_change(&output_changes[0]);
        assert_eq!(props.get("id"), Some(&ElementValue::Integer(1_i64)));

        let mut expected_map = ElementPropertyMap::new();
        expected_map.insert("key", ElementValue::String(Arc::from("value")));
        expected_map.insert("num", ElementValue::Integer(123_i64));
        let expected_obj = ElementValue::Object(expected_map);

        assert_property(props, "data_json", &expected_obj);
    }

    #[tokio::test]
    async fn test_parse_json_array_success_output_prop() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "raw_list",
            "output_property": "parsed_list"
        }));
        let middleware = factory
            .create(&config)
            .expect("Failed to create middleware");
        let index = InMemoryElementIndex::new();

        let input_change = create_node_insert_change(json!({
            "name": "test",
            "raw_list": r#"[1, "two", true, null]"#
        }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);

        let props = get_props_from_change(&output_changes[0]);
        assert_property(props, "name", &ElementValue::String(Arc::from("test")));
        assert_property(
            props,
            "raw_list",
            &ElementValue::String(Arc::from(r#"[1, "two", true, null]"#)),
        );

        let expected_list = ElementValue::List(vec![
            ElementValue::Integer(1_i64),
            ElementValue::String(Arc::from("two")),
            ElementValue::Bool(true),
            ElementValue::Null,
        ]);
        assert_property(props, "parsed_list", &expected_list);
    }

    #[tokio::test]
    async fn test_parse_json_output_prop_collision() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "json_str",
            "output_property": "parsed_data"
        }));
        let middleware = factory
            .create(&config)
            .expect("Failed to create middleware");
        let index = InMemoryElementIndex::new();

        let input_change = create_node_insert_change(json!({
            "json_str": r#"{"new": true}"#,
            "parsed_data": "old_value"
        }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);

        let props = get_props_from_change(&output_changes[0]);
        assert_property(
            props,
            "json_str",
            &ElementValue::String(Arc::from(r#"{"new": true}"#)),
        );

        let mut expected_map = ElementPropertyMap::new();
        expected_map.insert("new", ElementValue::Bool(true));
        let expected_obj = ElementValue::Object(expected_map);

        assert_property(props, "parsed_data", &expected_obj);
    }

    #[tokio::test]
    async fn test_update_change() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data",
        }));
        let middleware = factory
            .create(&config)
            .expect("Failed to create middleware");
        let index = InMemoryElementIndex::new();

        let input_change = create_node_update_change(json!({
            "data": r#"{"status": "updated"}"#
        }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert!(matches!(output_changes[0], SourceChange::Update { .. }));

        let props = get_props_from_change(&output_changes[0]);
        let mut expected_map = ElementPropertyMap::new();
        expected_map.insert("status", ElementValue::String(Arc::from("updated")));
        let expected_obj = ElementValue::Object(expected_map);

        assert_property(props, "data", &expected_obj);
    }

    #[tokio::test]
    async fn test_delete_change_passthrough() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "data" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_delete_change();
        let original_change_clone = input_change.clone();

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert_eq!(output_changes[0], original_change_clone);
    }

    #[tokio::test]
    async fn test_parse_json_primitive_success() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "value" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();

        // Test number
        let input_num = create_node_insert_change(json!({ "value": "123.45" }));
        let result_num = middleware.process(input_num, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_num[0]),
            "value",
            &ElementValue::Float(OrderedFloat(123.45)),
        );

        // Test string
        let input_str = create_node_insert_change(json!({ "value": r#""hello""# }));
        let result_str = middleware.process(input_str, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_str[0]),
            "value",
            &ElementValue::String(Arc::from("hello")),
        );

        // Test null
        let input_null = create_node_insert_change(json!({ "value": "null" }));
        let result_null = middleware.process(input_null, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_null[0]),
            "value",
            &ElementValue::Null,
        );
    }

    #[tokio::test]
    async fn test_parse_json_boolean_values() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "is_active" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();

        // Test true
        let input_true = create_node_insert_change(json!({ "is_active": "true" }));
        let result_true = middleware.process(input_true, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_true[0]),
            "is_active",
            &ElementValue::Bool(true),
        );

        // Test false
        let input_false = create_node_insert_change(json!({ "is_active": "false" }));
        let result_false = middleware.process(input_false, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_false[0]),
            "is_active",
            &ElementValue::Bool(false),
        );
    }

    #[tokio::test]
    async fn test_parse_json_nested_object() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "nested_data" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(json!({
            "nested_data": r#"{"level1": {"level2": "deep"}}"#
        }));

        let result = middleware.process(input_change, &index).await.unwrap();
        let mut level2_map = ElementPropertyMap::new();
        level2_map.insert("level2", ElementValue::String(Arc::from("deep")));
        let mut level1_map = ElementPropertyMap::new();
        level1_map.insert("level1", ElementValue::Object(level2_map));
        let expected_obj = ElementValue::Object(level1_map);

        assert_property(
            get_props_from_change(&result[0]),
            "nested_data",
            &expected_obj,
        );
    }

    #[tokio::test]
    async fn test_parse_json_mixed_array() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "mixed" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(json!({
            "mixed": r#"[1, "hello", null, true, {"nested": 3.14}, [10]]"#
        }));

        let result = middleware.process(input_change, &index).await.unwrap();
        let mut nested_map = ElementPropertyMap::new();
        nested_map.insert("nested", ElementValue::Float(OrderedFloat(3.14)));

        let expected_list = ElementValue::List(vec![
            ElementValue::Integer(1_i64),
            ElementValue::String(Arc::from("hello")),
            ElementValue::Null,
            ElementValue::Bool(true),
            ElementValue::Object(nested_map),
            ElementValue::List(vec![ElementValue::Integer(10_i64)]),
        ]);
        assert_property(get_props_from_change(&result[0]), "mixed", &expected_list);
    }

    #[tokio::test]
    async fn test_parse_json_empty_object_and_array() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "empty_data" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();

        // Empty object
        let input_obj = create_node_insert_change(json!({ "empty_data": "{}" }));
        let result_obj = middleware.process(input_obj, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_obj[0]),
            "empty_data",
            &ElementValue::Object(ElementPropertyMap::new()),
        );

        // Empty array
        let input_arr = create_node_insert_change(json!({ "empty_data": "[]" }));
        let result_arr = middleware.process(input_arr, &index).await.unwrap();
        assert_property(
            get_props_from_change(&result_arr[0]),
            "empty_data",
            &ElementValue::List(vec![]),
        );
    }

    #[tokio::test]
    async fn test_parse_json_with_unicode_and_escapes() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({ "target_property": "special" }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let json_string = r#"{"text": "你好世界 \" \\ \n end"}"#;
        let input_change = create_node_insert_change(json!({ "special": json_string }));

        let result = middleware.process(input_change, &index).await.unwrap();
        let mut expected_map = ElementPropertyMap::new();
        expected_map.insert(
            "text",
            ElementValue::String(Arc::from("你好世界 \" \\ \n end")),
        );
        let expected_obj = ElementValue::Object(expected_map);

        assert_property(get_props_from_change(&result[0]), "special", &expected_obj);
    }

    // --- Error Handling Tests (Skip Mode) ---

    #[tokio::test]
    async fn test_skip_on_missing_property() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "non_existent",
            "on_error": "skip"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(json!({ "id": 1 }));
        let original_change_clone = input_change.clone();

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert_eq!(output_changes[0], original_change_clone);
    }

    #[tokio::test]
    async fn test_skip_on_invalid_type() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data",
            "on_error": "skip"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(json!({ "data": 123 }));
        let original_change_clone = input_change.clone();

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert_eq!(output_changes[0], original_change_clone);
    }

    #[tokio::test]
    async fn test_skip_on_parse_error() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "invalid_json",
            "on_error": "skip"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change =
            create_node_insert_change(json!({ "invalid_json": r#"{"key": "value""# }));
        let original_change_clone = input_change.clone();

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok(), "Expected success in skip mode");
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert_eq!(output_changes[0], original_change_clone);
    }

    #[tokio::test]
    async fn test_skip_on_size_exceeded() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "large_json",
            "max_json_size": 10,
            "on_error": "skip"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(
            json!({ "large_json": r#"{"a": "this is definitely more than 10 bytes"}"# }),
        );
        let original_change_clone = input_change.clone();

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert_eq!(output_changes[0], original_change_clone);
    }

    #[tokio::test]
    async fn test_skip_on_deep_nesting() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "deep_json",
            "on_error": "skip"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let deep_json = (0..25).fold("null".to_string(), |acc, _| format!(r#"{{"k":{}}}"#, acc));
        let input_change = create_node_insert_change(json!({ "deep_json": deep_json }));
        let original_change_clone = input_change.clone();

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_ok());
        let output_changes = result.unwrap();
        assert_eq!(output_changes.len(), 1);
        assert_eq!(output_changes[0], original_change_clone);
    }

    // --- Error Handling Tests (Fail Mode) ---

    #[tokio::test]
    async fn test_fail_on_missing_property() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "non_existent",
            "on_error": "fail"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(json!({ "id": 1 }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("[Error: MissingProperty]"));
        assert!(err_msg.contains("Target property 'non_existent' not found"));
    }

    #[tokio::test]
    async fn test_fail_on_invalid_type() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data",
            "on_error": "fail"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change = create_node_insert_change(json!({ "data": 123 }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("[test_parse_json]"));
        assert!(err_msg.contains("[Error: InvalidType]"));
        assert!(err_msg.contains("Target property 'data' is not a String (Type: Integer)"));
    }

    #[tokio::test]
    async fn test_fail_on_parse_error() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "invalid_json",
            "on_error": "fail"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change =
            create_node_insert_change(json!({ "invalid_json": r#"{"key": "value""# }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("[Error: ParseError]"));
        assert!(err_msg.contains("Failed to parse JSON string"));
    }

    #[tokio::test]
    async fn test_fail_on_size_exceeded() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "large_json",
            "max_json_size": 10,
            "on_error": "fail"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let json_str = r#"{"a": "this is definitely more than 10 bytes"}"#;
        let input_change = create_node_insert_change(json!({ "large_json": json_str }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("[test_parse_json]"));
        assert!(err_msg.contains("[Error: SizeExceeded]"));
        assert!(err_msg.contains(&format!(
            "JSON string in property 'large_json' exceeds maximum allowed size ({} > 10 bytes)",
            json_str.len()
        )));
    }

    #[tokio::test]
    async fn test_fail_on_deep_nesting() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "deep_json",
            "on_error": "fail"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let deep_json = (0..25).fold("null".to_string(), |acc, _| format!(r#"{{"k":{}}}"#, acc));
        let input_change = create_node_insert_change(json!({ "deep_json": deep_json }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("[Error: DeepNesting]"));
        assert!(err_msg.contains("JSON nesting depth"));
    }

    #[tokio::test]
    async fn test_fail_on_invalid_input_control_chars() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "bad_json",
            "on_error": "fail"
        }));
        let middleware = factory.create(&config).unwrap();
        let index = InMemoryElementIndex::new();
        let input_change =
            create_node_insert_change(json!({ "bad_json": "{\"key\": \"value\0\"}" }));

        let result = middleware.process(input_change, &index).await;
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("[Error: InvalidInput]"));
        assert!(err_msg.contains("JSON string contains invalid control characters"));
    }
}

mod factory {
    use super::*;

    #[test]
    fn construct_parse_json_middleware_minimal() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data"
        }));
        let result = factory.create(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn construct_parse_json_middleware_full() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "input",
            "output_property": "output",
            "on_error": "fail",
            "max_json_size": 50000,
            "max_nesting_depth": 30
        }));
        let result = factory.create(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn fail_missing_target_property() {
        let factory = ParseJsonFactory::default();
        let config_map =
            serde_json::Map::from_iter(vec![("output_property".to_string(), json!("output"))]);
        let config = SourceMiddlewareConfig {
            name: Arc::from("test_parse_json"),
            kind: "parse_json".into(),
            config: config_map,
        };
        let result = factory.create(&config);
        assert!(result.is_err());
        assert!(
            matches!(result.err().unwrap(), MiddlewareSetupError::InvalidConfiguration(msg) if msg.contains("missing field `target_property`"))
        );
    }

    #[test]
    fn fail_empty_target_property() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": ""
        }));
        let result = factory.create(&config);
        assert!(result.is_err());
        assert!(
            matches!(result.err().unwrap(), MiddlewareSetupError::InvalidConfiguration(msg) if msg.contains("Missing or empty 'target_property' field"))
        );
    }

    #[test]
    fn fail_empty_output_property() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "input",
            "output_property": ""
        }));
        let result = factory.create(&config);
        assert!(result.is_err());
        assert!(
            matches!(result.err().unwrap(), MiddlewareSetupError::InvalidConfiguration(msg) if msg.contains("'output_property' cannot be empty if provided"))
        );
    }

    #[test]
    fn test_valid_on_error_values() {
        let factory = ParseJsonFactory::default();
        let config_skip = create_mw_config(json!({
            "target_property": "data",
            "on_error": "skip"
        }));
        assert!(factory.create(&config_skip).is_ok());

        let config_fail = create_mw_config(json!({
            "target_property": "data",
            "on_error": "fail"
        }));
        assert!(factory.create(&config_fail).is_ok());
    }

    #[test]
    fn fail_invalid_on_error_value() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data",
            "on_error": "ignore"
        }));
        let result = factory.create(&config);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains("unknown variant"));
        assert!(err_msg.contains("ignore"));
        assert!(err_msg.contains("skip") && err_msg.contains("fail"));
    }

    #[test]
    fn fail_unknown_config_field() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data",
            "unknown_field": true
        }));
        let result = factory.create(&config);
        assert!(result.is_err());
        assert!(
            matches!(result.err().unwrap(), MiddlewareSetupError::InvalidConfiguration(msg) if msg.contains("unknown field `unknown_field`"))
        );
    }

    #[test]
    fn test_default_max_size() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data"
        }));
        let result = factory.create(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn fail_zero_max_size() {
        let factory = ParseJsonFactory::default();
        let config = create_mw_config(json!({
            "target_property": "data",
            "max_json_size": 0
        }));
        let result = factory.create(&config);
        assert!(result.is_err());
        assert!(
            matches!(result.err().unwrap(), MiddlewareSetupError::InvalidConfiguration(msg) if msg.contains("'max_json_size' must be greater than 0"))
        );
    }
}
