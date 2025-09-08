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

//! # Decoder Middleware Tests

use std::sync::Arc;

use crate::decoder::DecoderFactory;
use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{FutureElementRef, MiddlewareError, MiddlewareSetupError, SourceMiddlewareFactory},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp,
        ElementValue, SourceChange, SourceMiddlewareConfig,
    },
};
use serde_json::{json, Value};

// --- Test Helpers ---

fn create_mw_config(config_json: Value) -> SourceMiddlewareConfig {
    SourceMiddlewareConfig {
        name: "test_decoder".into(),
        kind: "decoder".into(),
        config: config_json
            .as_object()
            .expect("Config JSON must be an object")
            .clone(),
    }
}

fn create_node_insert_change(props: Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "node1"),
                labels: vec!["TestNode".into()].into(),
                effective_from: 0,
            },
            properties: props.into(),
        },
    }
}

fn create_node_update_change(props: Value) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "node1"),
                labels: vec!["TestNode".into()].into(),
                effective_from: 1, // Different time from insert
            },
            properties: props.into(),
        },
    }
}

fn create_relation_insert_change(props: Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "rel1"),
                labels: vec!["CONNECTS".into()].into(),
                effective_from: 0,
            },
            properties: props.into(),
            in_node: ElementReference::new("test_source", "node1"),
            out_node: ElementReference::new("test_source", "node2"),
        },
    }
}

fn create_delete_change() -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", "node1"),
            labels: vec!["TestNode".into()].into(),
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

fn get_props_from_change(change: &SourceChange) -> &ElementPropertyMap {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_properties()
        }
        _ => panic!("Expected Insert or Update change to get properties"),
    }
}

// --- Test Modules ---

mod process {
    use super::*;

    #[tokio::test]
    async fn test_base64_decode_success_overwrite() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "SGVsbG8gV29ybGQh" // "Hello World!"
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Hello World!".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_base64_decode_success_output_prop() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "output_property": "decoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "SGVsbG8gV29ybGQh", // "Hello World!"
            "other_prop": 123
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("SGVsbG8gV29ybGQh".into()))
        );
        assert_eq!(
            props.get("decoded_value"),
            Some(&ElementValue::String("Hello World!".into()))
        );
        assert_eq!(props.get("other_prop"), Some(&ElementValue::Integer(123)));

        assert_eq!(props.map_iter(|_, _| ()).count(), 3);
    }

    #[tokio::test]
    async fn test_base64url_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64url",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "SGVsbG8gV29ybGQ_" // "Hello World?" (Base64URL for "Hello World?")
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Hello World?".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_hex_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "hex",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "48656c6c6f20576f726c6421" // "Hello World!"
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Hello World!".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_url_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "url",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "Hello%20World%21" // "Hello World!"
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Hello World!".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_json_escape_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "json_escape",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": r#"{\"key\": \"value with \\\"quotes\\\" and \\\\escapes\"}"#
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            // The expected result after JSON un-escaping
            Some(&ElementValue::String(
                r#"{"key": "value with \"quotes\" and \\escapes"}"#.into()
            ))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_decode_failure_skip() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "on_error": "skip" // Explicit skip
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "Invalid Base64 ---"
        }));

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);
        // Value should remain unchanged
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Invalid Base64 ---".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_decode_failure_fail_default() {
        // Default on_error is Fail
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "Invalid Base64 ---"
        }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Failed to decode property 'encoded_value'"));
                assert!(msg.contains("base64")); // Check encoding type in message
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_missing_target_property_skip() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "missing_prop",
            "on_error": "skip"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "other_prop": "value" }));

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1); // Should pass through unchanged
        let props = get_props_from_change(&result[0]);

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
        assert_eq!(
            props.get("other_prop"),
            Some(&ElementValue::String("value".into()))
        );
        assert!(props.get("missing_prop").is_none()); // Ensure missing prop wasn't added
    }

    #[tokio::test]
    async fn test_missing_target_property_fail_default() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "missing_prop"
            // on_error defaults to fail
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "other_prop": "value" }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Target property 'missing_prop' not found"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_wrong_target_property_type_skip() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type_prop",
            "on_error": "skip"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "wrong_type_prop": 12345 }));

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1); // Should pass through unchanged
        let props = get_props_from_change(&result[0]);

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
        assert_eq!(
            props.get("wrong_type_prop"),
            Some(&ElementValue::Integer(12345))
        );
    }

    #[tokio::test]
    async fn test_wrong_target_property_type_fail_default() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type_prop"
            // on_error defaults to fail
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "wrong_type_prop": true }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Target property 'wrong_type_prop' is not a string value"));
                assert!(msg.contains("(Type: Bool)")); // Check type name in message
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_strip_quotes_enabled() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "strip_quotes": true
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "\"SGVsbG8gV29ybGQh\"" // Base64("Hello World!") with quotes
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Hello World!".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_strip_quotes_disabled_default() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
            // strip_quotes defaults to false
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Base64 encoding of the string "\"Secret Message\"" (including the quotes)
        let source_change = create_node_insert_change(json!({
            "encoded_value": "IlNlY3JldCBNZXNzYWdlIg=="
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        // Expect the decoded string to still contain the quotes
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("\"Secret Message\"".into()))
        );

        assert_eq!(props.map_iter(|_, _| ()).count(), 1);
    }

    #[tokio::test]
    async fn test_size_limit_exceeded_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "max_size_bytes": 5, // Very small limit
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "SGVsbG8gV29ybGQh" // "Hello World!" (16 bytes)
        }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("exceeds size limit (16 > 5)"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_size_limit_exceeded_skip() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "max_size_bytes": 5, // Very small limit
            "on_error": "skip"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "encoded_value": "SGVsbG8gV29ybGQh" // "Hello World!" (16 bytes)
        }));

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        // Assert that the change is passed through unmodified
        assert_eq!(result[0], source_change);
    }

    #[tokio::test]
    async fn test_output_property_overwrite() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "hex",
            "target_property": "raw",
            "output_property": "decoded"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "raw": "48656c6c6f", // "Hello"
            "decoded": "old_value" // Existing property that should be overwritten
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("raw"),
            Some(&ElementValue::String("48656c6c6f".into()))
        );
        assert_eq!(
            props.get("decoded"),
            Some(&ElementValue::String("Hello".into())) // Assert overwritten value
        );
        assert_eq!(props.map_iter(|_, _| ()).count(), 2);
    }

    #[tokio::test]
    async fn test_process_relation_element_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "url",
            "target_property": "data"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_relation_insert_change(json!({
            "data": "Relation%20Data" // "Relation Data"
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert {
                element: Element::Relation { properties, .. },
            } => {
                assert_eq!(
                    properties.get("data"),
                    Some(&ElementValue::String("Relation Data".into()))
                );
                assert_eq!(properties.map_iter(|_, _| ()).count(), 1);
            }
            _ => panic!("Expected Insert Relation change"),
        }
    }

    #[tokio::test]
    async fn test_process_update_change_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_update_change(json!({
            "encoded_value": "VXBkYXRlZCE=" // "Updated!"
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Update { element } => {
                let props = element.get_properties();
                assert_eq!(
                    props.get("encoded_value"),
                    Some(&ElementValue::String("Updated!".into()))
                );
            }
            _ => panic!("Expected Update change"),
        }
    }

    #[tokio::test]
    async fn test_process_delete_change_passthrough() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_delete_change();

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], source_change); // Assert passthrough
    }

    #[tokio::test]
    async fn test_process_future_change_passthrough() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
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
        assert_eq!(result[0], source_change); // Assert passthrough
    }

    #[tokio::test]
    async fn test_decode_base64_invalid_utf8_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Base64 encoding of the invalid UTF-8 byte sequence [0x80]
        let source_change = create_node_insert_change(json!({ "encoded_value": "gA==" }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Failed to decode property 'encoded_value'"));
                assert!(msg.contains("base64"));
                assert!(msg.contains("Decoded bytes are not valid UTF-8"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_decode_base64url_invalid_utf8_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64url",
            "target_property": "encoded_value",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Base64URL encoding of the invalid UTF-8 byte sequence [0x80] (no padding)
        let source_change = create_node_insert_change(json!({ "encoded_value": "gA" }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Failed to decode property 'encoded_value'"));
                assert!(msg.contains("base64url"));
                assert!(msg.contains("Decoded bytes are not valid UTF-8"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_decode_hex_invalid_utf8_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "hex",
            "target_property": "encoded_value",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Hex encoding of the invalid UTF-8 byte sequence [0x80]
        let source_change = create_node_insert_change(json!({ "encoded_value": "80" }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Failed to decode property 'encoded_value'"));
                assert!(msg.contains("hex"));
                assert!(msg.contains("Decoded bytes are not valid UTF-8"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_decode_url_invalid_encoding_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "url",
            "target_property": "encoded_value",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Use a sequence that decodes to invalid UTF-8 (%80)
        let source_change =
            create_node_insert_change(json!({ "encoded_value": "invalid%80sequence" }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Failed to decode property 'encoded_value'"));
                assert!(msg.contains("url"));
                assert!(msg.contains("Invalid URL encoding: invalid utf-8 sequence"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_decode_json_escape_invalid_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "json_escape",
            "target_property": "encoded_value",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Invalid unicode escape sequence
        let source_change =
            create_node_insert_change(json!({ "encoded_value": r"Invalid \uDEFG sequence" }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Failed to decode property 'encoded_value'"));
                assert!(msg.contains("json_escape"));
                assert!(msg.contains("Invalid JSON escape sequence"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_strip_quotes_no_quotes_present() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "strip_quotes": true
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Correctly encoded, no quotes
        let source_change = create_node_insert_change(json!({
            "encoded_value": "SGVsbG8gV29ybGQh" // "Hello World!"
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let props = get_props_from_change(&result[0]);
        assert_eq!(
            props.get("encoded_value"),
            Some(&ElementValue::String("Hello World!".into()))
        );
    }

    #[tokio::test]
    async fn test_strip_quotes_mismatched_quotes() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "strip_quotes": true,
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        // Input that does not start AND end with quotes, so strip_quotes should do nothing.
        // The remaining value "abc" is invalid Base64.
        let source_change = create_node_insert_change(json!({
            "encoded_value": "abc"
        }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err()); // Decoding should fail
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Invalid base64 encoding"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_wrong_target_property_type_fail_float() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type_prop",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "wrong_type_prop": 123.45 }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Target property 'wrong_type_prop' is not a string value"));
                assert!(msg.contains("(Type: Float)"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_wrong_target_property_type_fail_list() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type_prop",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "wrong_type_prop": [1, 2, 3] }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Target property 'wrong_type_prop' is not a string value"));
                assert!(msg.contains("(Type: List)"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_wrong_target_property_type_fail_object() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type_prop",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "wrong_type_prop": {"a": 1} }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Target property 'wrong_type_prop' is not a string value"));
                assert!(msg.contains("(Type: Object)"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    async fn test_wrong_target_property_type_fail_null() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type_prop",
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({ "wrong_type_prop": null }));

        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Target property 'wrong_type_prop' is not a string value"));
                assert!(msg.contains("(Type: Null)"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }
}

mod factory {
    use super::*;

    #[test]
    fn construct_decoder_middleware_minimal() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "data"
        });
        let mw_config = create_mw_config(config);
        assert!(subject.create(&mw_config).is_ok());
    }

    #[test]
    fn construct_decoder_middleware_full() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "hex",
            "target_property": "input",
            "output_property": "output",
            "strip_quotes": true,
            "on_error": "skip" // Explicitly testing skip here
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_ok());
    }

    #[test]
    fn fail_missing_target_property() {
        let subject = DecoderFactory::new();
        let config = json!({ "encoding_type": "base64" }); // Missing target_property
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                // Check serde's error message for missing field
                assert!(msg.contains("missing field `target_property`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_empty_target_property() {
        let subject = DecoderFactory::new();
        let config = json!({ "encoding_type": "base64", "target_property": "" });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                // Check our custom validation message
                assert!(msg.contains("Missing or empty 'target_property'"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_missing_encoding_type() {
        let subject = DecoderFactory::new();
        let config = json!({ "target_property": "data" }); // Missing encoding_type
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("missing field `encoding_type`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_invalid_encoding_type() {
        let subject = DecoderFactory::new();
        let config = json!({ "encoding_type": "invalid", "target_property": "data" });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                // Check serde's error message for unknown variant
                assert!(msg.contains("unknown variant `invalid`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_empty_output_property() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "input",
            "output_property": "" // Empty output property
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                // Check our custom validation message
                assert!(msg.contains("'output_property' cannot be empty"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_all_valid_encoding_types() {
        let subject = DecoderFactory::new();
        let types = ["base64", "base64url", "hex", "url", "json_escape"];
        for enc_type in types {
            let config = json!({
                "encoding_type": enc_type,
                "target_property": "data"
            });
            let mw_config = create_mw_config(config);
            assert!(
                subject.create(&mw_config).is_ok(),
                "Failed for encoding_type: {enc_type}"
            );
        }
    }

    #[test]
    fn test_all_valid_on_error_values() {
        let subject = DecoderFactory::new();
        let values = ["skip", "fail"];
        for val in values {
            let config = json!({
                "encoding_type": "base64",
                "target_property": "data",
                "on_error": val
            });
            let mw_config = create_mw_config(config);
            assert!(
                subject.create(&mw_config).is_ok(),
                "Failed for on_error: {val}"
            );
        }
        // Test default (fail) by omitting the field
        let config_default = json!({ "encoding_type": "base64", "target_property": "data" });
        let mw_config_default = create_mw_config(config_default);
        assert!(
            subject.create(&mw_config_default).is_ok(),
            "Failed for default on_error"
        );
    }

    #[test]
    fn fail_invalid_on_error_value() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "data",
            "on_error": "invalid_value" // Invalid enum variant
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("unknown variant `invalid_value`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_unknown_config_field() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "data",
            "unknown_field": 123 // Added deny_unknown_fields to config struct
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("unknown field `unknown_field`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_max_size_bytes_zero() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "data",
            "max_size_bytes": 0 // Invalid size
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("'max_size_bytes' must be greater than zero"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_factory_default_creation() {
        let subject = DecoderFactory::default(); // Use default constructor
        let config = json!({
            "encoding_type": "base64",
            "target_property": "data"
        });
        let mw_config = create_mw_config(config);
        assert!(subject.create(&mw_config).is_ok());
    }
}
