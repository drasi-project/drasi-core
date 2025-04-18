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
//!
//! This module contains comprehensive tests for the decoder middleware, verifying:
//!
//! - Support for multiple encoding types (base64, base64url, hex, url, json_escape)
//! - Error handling behavior with the `on_error` option
//! - Property handling including missing properties and wrong types
//! - JSON parsing and flattening capabilities
//! - Configuration validation

use std::sync::Arc;

use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{MiddlewareError, SourceMiddlewareFactory},
    models::{
        Element, ElementMetadata, ElementReference, ElementValue, SourceChange,
        SourceMiddlewareConfig,
    },
};
use serde_json::json;

use crate::decoder::DecoderFactory;

mod process {
    use super::*;

    /// Tests for successful base64 decoding
    #[tokio::test]
    async fn test_base64_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "SGVsbG8gV29ybGQh"  // Base64 encoded "Hello World!"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Hello World!".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for base64 decoding failure
    #[tokio::test]
    async fn test_base64_decode_failure() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "Invalid Base64 String"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Invalid Base64 String".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for base64url decoding success
    #[tokio::test]
    async fn test_base64url_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64url",
            "target_property": "encoded_value"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "SGVsbG8gV29ybGQh"  // Base64 URL-safe encoded "Hello World!"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Hello World!".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for hex decoding success
    #[tokio::test]
    async fn test_hex_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "hex",
            "target_property": "encoded_value"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "48656c6c6f20576f726c6421"  // Hex encoded "Hello World!"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Hello World!".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for hex decoding failure
    #[tokio::test]
    async fn test_hex_decode_failure() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "hex",
            "target_property": "encoded_value",
            "on_error": "fail"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "Not a valid hex string ZZ"  // Invalid hex
                })
                .into(),
            },
        };

        // This should fail with an error
        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("[test]"));
                assert!(msg.contains("hex encoding"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    /// Tests for URL decoding success
    #[tokio::test]
    async fn test_url_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "url",
            "target_property": "encoded_value"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "Hello%20World%21"  // URL encoded "Hello World!"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Hello World!".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for json_escape decoding success
    #[tokio::test]
    async fn test_json_escape_decode_success() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "json_escape",
            "target_property": "encoded_value"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "Hello\\u0020World\\u0021"  // JSON escaped "Hello World!"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Hello World!".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for json_escape decoding failure
    #[tokio::test]
    async fn test_json_escape_decode_failure() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "json_escape",
            "target_property": "encoded_value",
            "on_error": "fail"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "Invalid escape \\u00zz sequence"  // Invalid JSON escape
                })
                .into(),
            },
        };

        // This should fail with an error
        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("[test]"));
                assert!(msg.contains("JSON escape"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    // Tests for decoding failure when on_error is set to fail
    #[tokio::test]
    async fn test_decoder_with_on_error_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "on_error": "fail"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "Invalid Base64 String"
                })
                .into(),
            },
        };

        // This should fail with an error
        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("[test]"));
                assert!(msg.contains("Failed to decode property"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    /// Tests for failure when the target property is missing and on_error is set to skip
    #[tokio::test]
    async fn test_decoder_missing_property() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "missing_value",
            "on_error": "skip"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "some_other_value": "Some Value"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let value = element.get_properties().get("some_other_value").unwrap();
            assert_eq!(
                value,
                &ElementValue::String("Some Value".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for failure when the target property is missing and on_error is set to fail
    #[tokio::test]
    async fn test_decoder_missing_property_with_on_error_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "missing_value",
            "on_error": "fail"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "some_other_value": "Some Value"
                })
                .into(),
            },
        };

        // This should fail with an error since the property is missing
        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("[test]"));
                assert!(msg.contains("Property missing_value not found"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    /// Tests for failure when the target property is of the wrong type (a number for example)
    #[tokio::test]
    async fn test_decoder_wrong_type() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "wrong_type"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "wrong_type": 123
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let value = element.get_properties().get("wrong_type").unwrap();
            //  Using Integer variant
            assert_eq!(value, &ElementValue::Integer(123));
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for stripping quotes from the decoded value when the option is set
    #[tokio::test]
    async fn test_decoder_with_quotes() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "strip_quotes": true
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "\"SGVsbG8gV29ybGQh\""  // Base64 encoded "Hello World!" with quotes
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String("Hello World!".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for json parsing of the decoded value when the option is set
    #[tokio::test]
    async fn test_decoder_with_json_parsing() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "parse_json": true
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        // Base64 for {"name":"Alice","age":30,"active":true}
        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "eyJuYW1lIjoiQWxpY2UiLCJhZ2UiOjMwLCJhY3RpdmUiOnRydWV9"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            assert_eq!(
                decoded_value,
                &ElementValue::String(r#"{"name":"Alice","age":30,"active":true}"#.into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for the json parsing and flattening of the decoded value when the options are set
    #[tokio::test]
    async fn test_decoder_with_json_parsing_and_flattening() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "parse_json": true,
            "flatten": true,
            "output_prefix": "user_"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        // Base64 for {"name":"Alice","age":30,"active":true}
        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "eyJuYW1lIjoiQWxpY2UiLCJhZ2UiOjMwLCJhY3RpdmUiOnRydWV9"
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            // Original property should remain
            assert!(element.get_properties().get("encoded_value").is_some());

            // Check flattened properties with prefix
            let name = element.get_properties().get("user_name").unwrap();
            let age = element.get_properties().get("user_age").unwrap();
            let active = element.get_properties().get("user_active").unwrap();

            assert_eq!(name, &ElementValue::String("Alice".to_string().into()));
            assert_eq!(age, &ElementValue::Integer(30));
            assert_eq!(active, &ElementValue::String("true".to_string().into()));
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for json parsing failure when the option is set
    #[tokio::test]
    async fn test_decoder_with_invalid_json() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "parse_json": true
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        // Base64 for "not a valid json"
        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "bm90IGEgdmFsaWQganNvbg=="
                })
                .into(),
            },
        };

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();
        let change = &result[0];

        if let SourceChange::Insert { element } = change {
            let decoded_value = element.get_properties().get("encoded_value").unwrap();
            // Even though JSON parsing failed, it should still decode the base64
            assert_eq!(
                decoded_value,
                &ElementValue::String("not a valid json".to_string().into())
            );
        } else {
            panic!("Expected an Insert change");
        }
    }

    /// Tests for json parsing failure when the option is set and on_error is set to fail
    #[tokio::test]
    async fn test_decoder_with_invalid_json_on_error_fail() {
        let factory = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "parse_json": true,
            "on_error": "fail"
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        // Base64 for "not a valid json"
        let source_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node1"),
                    labels: vec!["TestNode".into()].into(),
                    effective_from: 0,
                },
                properties: json!({
                    "encoded_value": "bm90IGEgdmFsaWQganNvbg=="
                })
                .into(),
            },
        };

        // This should fail with an error since JSON parsing fails and on_error is set to fail
        let result = subject.process(source_change, element_index.as_ref()).await;
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("[test]"));
                assert!(msg.contains("Failed to parse JSON"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }
}

mod factory {
    use drasi_core::interface::MiddlewareSetupError;
    use drasi_core::interface::SourceMiddlewareFactory;
    use drasi_core::models::SourceMiddlewareConfig;
    use serde_json::json;

    use crate::decoder::{DecoderFactory, EncodingType};

    /// Tests for the DecoderFactory's ability to create a decoder middleware
    #[tokio::test]
    pub async fn construct_decoder_middleware() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value"
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.create(&mw_config).is_ok());
    }

    /// Tests for error handling when the target property is missing in the decoder config
    #[tokio::test]
    pub async fn missing_target_property() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64"
            // Missing "target_property"
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        // It should not panic, but return an error
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("target_property"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    /// Tests for error handling when the encoding type is missing in the decoder config
    #[tokio::test]
    pub async fn missing_encoding_type() {
        let subject = DecoderFactory::new();
        let config = json!({
            "target_property": "encoded_value"
            // Missing "encoding_type"
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        // It should not panic, but return an error
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("encoding_type"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    /// Tests whether decoder can be created with all encoding types
    #[tokio::test]
    pub async fn test_all_encoding_types() {
        let subject = DecoderFactory::new();

        // Test all valid encoding types
        let encoding_types = vec![
            ("base64", EncodingType::Base64),
            ("base64url", EncodingType::Base64url),
            ("hex", EncodingType::Hex),
            ("url", EncodingType::Url),
            ("json_escape", EncodingType::JsonEscape),
        ];

        for (type_str, _) in encoding_types {
            let config = json!({
                "encoding_type": type_str,
                "target_property": "encoded_value"
            });

            let mw_config = SourceMiddlewareConfig {
                name: format!("test_{}", type_str).into(),
                kind: "decoder".into(),
                config: config.as_object().unwrap().clone(),
            };

            let result = subject.create(&mw_config);
            assert!(
                result.is_ok(),
                "Failed to create decoder with encoding_type: {}",
                type_str
            );
        }
    }

    /// Tests for error handling when an invalid encoding type is provided
    #[tokio::test]
    pub async fn test_invalid_encoding_type() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "invalid_encoding", // Invalid encoding type
            "target_property": "encoded_value"
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        // It should return an error for invalid encoding type
        let result = subject.create(&mw_config);
        assert!(result.is_err());

        // The error message from serde might not contain "encoding_type" directly
        // Let's just check that it's an InvalidConfiguration error
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(_)) => {
                // This is the expected error type
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    /// Tests for error handling when the flatten option is set without parse_json
    #[tokio::test]
    pub async fn test_invalid_flatten_config() {
        let subject = DecoderFactory::new();
        let config = json!({
            "encoding_type": "base64",
            "target_property": "encoded_value",
            "flatten": true,  // This requires parse_json:true
            "parse_json": false
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "decoder".into(),
            config: config.as_object().unwrap().clone(),
        };

        // It should return an error since flatten requires parse_json
        let result = subject.create(&mw_config);
        assert!(result.is_err());

        // Let's also check the error message
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("flatten"));
                assert!(msg.contains("parse_json"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }
}
