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

use drasi_middleware::decoder::DecoderFactory;
use serde_json::json;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::BTreeMap::new();
         $( map.insert($key.to_string().into(), $val); )*
         map
    }}
}

// Helper to create VariableValue::Null more easily
fn null_vv() -> VariableValue {
    VariableValue::Null
}

#[allow(clippy::print_stdout, clippy::unwrap_used, clippy::too_many_lines)]
pub async fn decoder(config: &(impl QueryTestConfig + Send)) {
    let mut middleware_registry = MiddlewareTypeRegistry::new();
    // Register the Decoder middleware factory
    middleware_registry.register(Arc::new(DecoderFactory::new()));
    let middleware_registry = Arc::new(middleware_registry);

    // --- Test Case 1: Base64 Decode, Overwrite Target ---
    println!("--- Test Case 1: Base64 Decode, Overwrite Target ---");
    let mw_name1 = "base64_overwrite";
    let query1 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name1,
            "base64",
            "encoded_data",
            None,
            false,
            "fail",
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name1));
        builder.build().await
    };

    // Insert node with base64 data
    let insert1 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node1"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node1",
                "encoded_data": "SGVsbG8gV29ybGQh", // "Hello World!"
                "other_prop": 123
            })),
        },
    };
    let result1 = query1.process_source_change(insert1).await.unwrap();
    println!("Result 1 (Insert): {:?}", result1);
    assert_eq!(result1.len(), 1);
    assert!(result1.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::from(json!("node1")),
            // encoded_data is overwritten with the decoded value
            "encoded_data" => VariableValue::from(json!("Hello World!")),
            "decoded_data" => null_vv(),
            "other_prop" => VariableValue::from(json!(123))
        )
    }));

    // --- Test Case 2: Hex Decode, Output Property, Strip Quotes ---
    println!("\n--- Test Case 2: Hex Decode, Output Property, Strip Quotes ---");
    let mw_name2 = "hex_output_strip";
    let query2 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name2,
            "hex",
            "encoded_data",
            Some("decoded_data"),
            true,
            "fail",
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name2));
        builder.build().await
    };

    // Insert node with hex data surrounded by quotes
    let insert2 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node2"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node2",
                // "Hex Data" encoded as hex, surrounded by literal quotes
                "encoded_data": "\"4865782044617461\"",
                "other_prop": 456
            })),
        },
    };
    let result2 = query2.process_source_change(insert2).await.unwrap();
    println!("Result 2 (Insert): {:?}", result2);
    assert_eq!(result2.len(), 1);
    assert!(result2.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::from(json!("node2")),
            // Original encoded_data remains
            "encoded_data" => VariableValue::from(json!("\"4865782044617461\"")),
            // decoded_data gets the result
            "decoded_data" => VariableValue::from(json!("Hex Data")),
            "other_prop" => VariableValue::from(json!(456))
        )
    }));

    // --- Test Case 3: URL Decode Failure, on_error: skip ---
    println!("\n--- Test Case 3: URL Decode Failure, on_error: skip ---");
    let mw_name3 = "url_fail_skip";
    let query3 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name3,
            "url",
            "encoded_data",
            Some("decoded_data"),
            false,
            "skip",
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name3));
        builder.build().await
    };

    // Insert node with invalid URL encoding
    let insert3 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node3"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node3",
                "encoded_data": "invalid%80sequence", // Invalid encoding
                "other_prop": 789
            })),
        },
    };
    let result3 = query3.process_source_change(insert3).await.unwrap();
    println!("Result 3 (Insert - Invalid URL, Skip): {:?}", result3);
    // Middleware should not modify the element
    assert_eq!(result3.len(), 1);
    assert!(result3.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::from(json!("node3")),
            "encoded_data" => VariableValue::from(json!("invalid%80sequence")), // Unchanged
            "decoded_data" => null_vv(), // Not set
            "other_prop" => VariableValue::from(json!(789))
        )
    }));

    // --- Test Case 4: JSON Escape Decode ---
    println!("\n--- Test Case 4: JSON Escape Decode ---");
    let mw_name4 = "json_escape_decode";
    let query4 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name4,
            "json_escape",
            "encoded_data",
            Some("decoded_data"),
            false,
            "fail",
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name4));
        builder.build().await
    };

    // Insert node with JSON escaped string
    let insert4 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node4"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node4",
                // Represents: "Line 1\nLine 2 with \"quotes\" and \\ backslash"
                "encoded_data": "Line 1\\nLine 2 with \\\"quotes\\\" and \\\\ backslash",
                "other_prop": 101
            })),
        },
    };
    let result4 = query4.process_source_change(insert4).await.unwrap();
    println!("Result 4 (Insert - JSON Escape): {:?}", result4);
    assert_eq!(result4.len(), 1);
    assert!(result4.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::from(json!("node4")),
            "encoded_data" => VariableValue::from(json!("Line 1\\nLine 2 with \\\"quotes\\\" and \\\\ backslash")),
            // Decoded value has a newline, quotes, and a backslash
            "decoded_data" => VariableValue::from(json!("Line 1\nLine 2 with \"quotes\" and \\ backslash")),
            "other_prop" => VariableValue::from(json!(101))
        )
    }));

    // --- Test Case 5: Missing Target Property, on_error: fail ---
    println!("\n--- Test Case 5: Missing Target Property, on_error: fail ---");
    let mw_name5 = "missing_prop_fail";
    let query5 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name5,
            "base64",
            "non_existent_prop",
            Some("decoded_data"),
            false,
            "fail",
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name5));
        builder.build().await
    };

    // Insert node without the target property
    let insert5 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node5"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node5",
                // "non_existent_prop" is missing
                "other_prop": 112
            })),
        },
    };
    let result5 = query5.process_source_change(insert5).await;
    println!("Result 5 (Insert - Missing Prop, Fail): {:?}", result5);
    // Expect an error because on_error is fail
    assert!(result5.is_err());
    assert!(result5
        .unwrap_err()
        .to_string()
        .contains("Target property 'non_existent_prop' not found"));

    // --- Test Case 6: Wrong Target Property Type, on_error: skip ---
    println!("\n--- Test Case 6: Wrong Target Property Type, on_error: skip ---");
    let mw_name6 = "wrong_type_skip";
    let query6 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name6,
            "base64",
            "encoded_data",
            Some("decoded_data"),
            false,
            "skip",
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name6));
        builder.build().await
    };

    // Insert node where target property is not a string
    let insert6 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node6"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node6",
                "encoded_data": 12345, // Not a string!
                "other_prop": 113
            })),
        },
    };
    let result6 = query6.process_source_change(insert6).await.unwrap();
    println!("Result 6 (Insert - Wrong Type, Skip): {:?}", result6);
    // Because on_error is skip, the element is added but not modified by middleware
    assert_eq!(result6.len(), 1);
    assert!(result6.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::from(json!("node6")),
            "encoded_data" => VariableValue::from(json!(12345)), // Unchanged
            "decoded_data" => null_vv(), // Not set
            "other_prop" => VariableValue::from(json!(113))
        )
    }));

    // --- Test Case 7: Size Limit Exceeded, on_error: fail ---
    println!("\n--- Test Case 7: Size Limit Exceeded, on_error: fail ---");
    let mw_name7 = "size_limit_fail";
    let query7 = {
        let mut builder = QueryBuilder::new(queries::decoder_query());
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_decoder_middleware(
            mw_name7,
            "base64",
            "encoded_data",
            Some("decoded_data"),
            false,
            "fail",
            Some(10),
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test", &queries::source_pipeline(mw_name7));
        builder.build().await
    };

    // Insert node with data exceeding the small limit
    let insert7 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node7"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node7",
                "encoded_data": "SGVsbG8gV29ybGQh", // "Hello World!" (16 bytes > 10)
                "other_prop": 114
            })),
        },
    };
    let result7 = query7.process_source_change(insert7).await;
    println!("Result 7 (Insert - Size Limit, Fail): {:?}", result7);
    // Expect an error because on_error is fail and size limit exceeded
    assert!(result7.is_err());
    assert!(result7
        .unwrap_err()
        .to_string()
        .contains("exceeds size limit"));

    // --- Test Case 8: Update existing node ---
    println!("\n--- Test Case 8: Update existing node ---");
    // Use query1 setup (Base64 Decode, Overwrite Target)
    // First, insert the initial state again
    let insert8_initial = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node8"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node8",
                "encoded_data": "SGVsbG8=", // "Hello"
                "other_prop": 200
            })),
        },
    };
    let result8_initial = query1.process_source_change(insert8_initial).await.unwrap();
    assert!(
        result8_initial.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "id" => VariableValue::from(json!("node8")),
                "encoded_data" => VariableValue::from(json!("Hello")), // Decoded
                "decoded_data" => null_vv(),
                "other_prop" => VariableValue::from(json!(200))
            )
        })
    );

    // Update the node with new encoded data
    let update8 = SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node8"),
                labels: vec!["TargetNode".into()].into(),
                effective_from: 1,
            },
            properties: ElementPropertyMap::from(json!({
                "id": "node8",
                "encoded_data": "R29vZGJ5ZQ==", // "Goodbye"
                "other_prop": 201 // Also update other prop
            })),
        },
    };
    let result8_update = query1.process_source_change(update8).await.unwrap();
    println!("Result 8 (Update): {:?}", result8_update);
    assert_eq!(result8_update.len(), 1);
    assert!(
        result8_update.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "id" => VariableValue::from(json!("node8")),
                "encoded_data" => VariableValue::from(json!("Hello")),
                "decoded_data" => null_vv(),
                "other_prop" => VariableValue::from(json!(200))
            ),
            after: variablemap!(
                "id" => VariableValue::from(json!("node8")),
                "encoded_data" => VariableValue::from(json!("Goodbye")),
                "decoded_data" => null_vv(),
                "other_prop" => VariableValue::from(json!(201))
            )
        })
    );

    // --- Test Case 9: Delete node ---
    println!("\n--- Test Case 9: Delete node ---");
    // Use query1 setup
    // Delete the node inserted in the previous step
    let delete9 = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", "node8"),
            labels: vec!["TargetNode".into()].into(),
            effective_from: 2,
        },
    };
    let result9_delete = query1.process_source_change(delete9).await.unwrap();
    println!("Result 9 (Delete): {:?}", result9_delete);
    assert_eq!(result9_delete.len(), 1);
    assert!(
        result9_delete.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "id" => VariableValue::from(json!("node8")),
                "encoded_data" => VariableValue::from(json!("Goodbye")),
                "decoded_data" => null_vv(),
                "other_prop" => VariableValue::from(json!(201))
            )
        })
    );
}
