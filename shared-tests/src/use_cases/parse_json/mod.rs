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

use drasi_middleware::parse_json::ParseJsonFactory;
use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::FunctionRegistry,
        variable_value::{float::Float, integer::Integer, VariableValue},
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

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

// Helper to create a basic node insert change
fn create_node_insert_change(id: &str, props_json: serde_json::Value) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    if let serde_json::Value::Object(obj) = props_json {
        for (k, v) in obj {
            props.insert(&k, ElementValue::from(&v));
        }
    }
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("TestNode")].into(),
                effective_from: 0,
            },
            properties: props,
        },
    }
}

// Helper to create a basic node update change
fn create_node_update_change(id: &str, props_json: serde_json::Value) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    if let serde_json::Value::Object(obj) = props_json {
        for (k, v) in obj {
            props.insert(&k, ElementValue::from(&v));
        }
    }
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("TestNode")].into(),
                effective_from: 0,
            },
            properties: props,
        },
    }
}

// Helper to create a basic node delete change
fn create_node_delete_change(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", id),
            labels: vec![Arc::from("TestNode")].into(),
            effective_from: 0,
        },
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used, clippy::too_many_lines)]
pub async fn parse_json_test(config: &(impl QueryTestConfig + Send)) {
    let mut middleware_registry = MiddlewareTypeRegistry::new();
    middleware_registry.register(Arc::new(ParseJsonFactory::new()));
    let middleware_registry = Arc::new(middleware_registry);

    // --- Test Case 1: Basic Parse & Overwrite ---
    println!("--- Test Case 1: Basic Parse & Overwrite ---");
    let mw_name1 = "parse_overwrite";
    let query1 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config =
            queries::create_parse_json_middleware(mw_name1, "target_prop", None, None, None, None);
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name1));
        builder.build().await
    };

    let insert1 = create_node_insert_change(
        "node:1",
        json!({ "id": "node:1", "target_prop": r#"{"key": "value", "num": 123}"# }),
    );
    let result1 = query1.process_source_change(insert1).await.unwrap();
    println!("Result 1 (Insert): {:?}", result1);
    assert_eq!(result1.len(), 1);
    let expected_vv_map1 = variablemap!(
        "key" => VariableValue::String("value".to_string()),
        "num" => VariableValue::Integer(Integer::from(123))
    );
    assert!(result1.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:1".to_string()),
            "target_prop" => VariableValue::Object(expected_vv_map1),
            "output_prop" => null_vv(),
            "other_prop" => null_vv()
        )
    }));

    // --- Test Case 2: Parse & Output Property ---
    println!("\n--- Test Case 2: Parse & Output Property ---");
    let mw_name2 = "parse_output";
    let query2 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name2,
            "target_prop",
            Some("output_prop"),
            None,
            None,
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name2));
        builder.build().await
    };

    let insert2 = create_node_insert_change(
        "node:2",
        json!({ "id": "node:2", "target_prop": r#"[1, "two", true, null, 1.234]"# }),
    );
    let result2 = query2.process_source_change(insert2).await.unwrap();
    println!("Result 2 (Insert): {:?}", result2);
    assert_eq!(result2.len(), 1);
    let expected_vv_list2 = vec![
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("two".to_string()),
        VariableValue::Bool(true),
        null_vv(),
        VariableValue::Float(Float::from(1.234)),
    ];
    assert!(result2.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:2".to_string()),
            "target_prop" => VariableValue::String(r#"[1, "two", true, null, 1.234]"#.to_string()),
            "output_prop" => VariableValue::List(expected_vv_list2),
            "other_prop" => null_vv()
        )
    }));

    // --- Test Case 3: Output Property Collision ---
    println!("\n--- Test Case 3: Output Property Collision ---");
    // Use query2 setup
    let insert3 = create_node_insert_change(
        "node:3",
        json!({
            "id": "node:3",
            "target_prop": r#"{"new": true}"#,
            "output_prop": "old_value"
        }),
    );
    let result3 = query2.process_source_change(insert3).await.unwrap();
    println!("Result 3 (Insert - Collision): {:?}", result3);
    assert_eq!(result3.len(), 1);
    let expected_vv_map3 = variablemap!(
        "new" => VariableValue::Bool(true)
    );
    assert!(result3.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:3".to_string()),
            "target_prop" => VariableValue::String(r#"{"new": true}"#.to_string()),
            "output_prop" => VariableValue::Object(expected_vv_map3),
            "other_prop" => null_vv()
        )
    }));

    // --- Test Case 4: Update Change ---
    println!("\n--- Test Case 4: Update Change ---");
    // Use query1 setup
    let insert4_initial = create_node_insert_change(
        "node:4",
        json!({ "id": "node:4", "target_prop": r#"{"status": "initial"}"# }),
    );
    let result4_initial = query1.process_source_change(insert4_initial).await.unwrap();
    let initial_vv_map4 = variablemap!(
        "status" => VariableValue::String("initial".to_string())
    );
    assert!(
        result4_initial.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "id" => VariableValue::String("node:4".to_string()),
                "target_prop" => VariableValue::Object(initial_vv_map4.clone()),
                "output_prop" => null_vv(),
                "other_prop" => null_vv()
            )
        })
    );

    let update4 = create_node_update_change(
        "node:4",
        json!({ "id": "node:4", "target_prop": r#"{"status": "updated"}"# }),
    );
    let result4_update = query1.process_source_change(update4).await.unwrap();
    println!("Result 4 (Update): {:?}", result4_update);
    assert_eq!(result4_update.len(), 1);
    let updated_vv_map4 = variablemap!(
        "status" => VariableValue::String("updated".to_string())
    );
    assert!(
        result4_update.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "id" => VariableValue::String("node:4".to_string()),
                "target_prop" => VariableValue::Object(initial_vv_map4),
                "output_prop" => null_vv(),
                "other_prop" => null_vv()
            ),
            after: variablemap!(
                "id" => VariableValue::String("node:4".to_string()),
                "target_prop" => VariableValue::Object(updated_vv_map4.clone()),
                "output_prop" => null_vv(),
                "other_prop" => null_vv()
            )
        })
    );

    // --- Test Case 5: Delete Change ---
    println!("\n--- Test Case 5: Delete Change ---");
    // Use query1 setup
    let delete5 = create_node_delete_change("node:4"); // Delete the node from previous step
    let result5_delete = query1.process_source_change(delete5).await.unwrap();
    println!("Result 5 (Delete): {:?}", result5_delete);
    assert_eq!(result5_delete.len(), 1);
    let deleted_vv_map5 = updated_vv_map4;
    assert!(
        result5_delete.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
                "id" => VariableValue::String("node:4".to_string()),
                "target_prop" => VariableValue::Object(deleted_vv_map5),
                "output_prop" => null_vv(),
                "other_prop" => null_vv()
            ),
        })
    );

    // --- Test Case 6: Error Handling - Skip (Invalid JSON) ---
    println!("\n--- Test Case 6: Error Handling - Skip (Invalid JSON) ---");
    let mw_name6 = "parse_skip_invalid";
    let query6 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name6,
            "target_prop",
            None,
            Some("skip"),
            None,
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name6));
        builder.build().await
    };
    let insert6 = create_node_insert_change(
        "node:6",
        json!({ "id": "node:6", "target_prop": r#"{"key": "value""# }),
    );
    let result6 = query6.process_source_change(insert6).await.unwrap();
    println!("Result 6 (Skip Invalid): {:?}", result6);
    assert_eq!(result6.len(), 1);
    assert!(result6.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:6".to_string()),
            "target_prop" => VariableValue::String(r#"{"key": "value""#.to_string()),
            "output_prop" => null_vv(),
            "other_prop" => null_vv()
        )
    }));

    // --- Test Case 7: Error Handling - Fail (Invalid JSON) ---
    println!("\n--- Test Case 7: Error Handling - Fail (Invalid JSON) ---");
    let mw_name7 = "parse_fail_invalid";
    let query7 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name7,
            "target_prop",
            None,
            Some("fail"),
            None,
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name7));
        builder.build().await
    };
    let insert7 = create_node_insert_change(
        "node:7",
        json!({ "id": "node:7", "target_prop": r#"{"key": "value""# }),
    );
    let result7 = query7.process_source_change(insert7).await;
    println!("Result 7 (Fail Invalid): {:?}", result7);
    assert!(result7.is_err());
    assert!(result7.unwrap_err().to_string().contains("ParseError"));

    // --- Test Case 8: Error Handling - Skip (Missing Property) ---
    println!("\n--- Test Case 8: Error Handling - Skip (Missing Property) ---");
    // Use query6 setup
    let insert8 = create_node_insert_change(
        "node:8",
        json!({ "id": "node:8", "other_prop": "some_value" }),
    );
    let result8 = query6.process_source_change(insert8).await.unwrap();
    println!("Result 8 (Skip Missing): {:?}", result8);
    assert_eq!(result8.len(), 1);
    assert!(result8.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:8".to_string()),
            "target_prop" => null_vv(),
            "output_prop" => null_vv(),
            "other_prop" => VariableValue::String("some_value".to_string())
        )
    }));

    // --- Test Case 9: Error Handling - Fail (Missing Property) ---
    println!("\n--- Test Case 9: Error Handling - Fail (Missing Property) ---");
    // Use query7 setup
    let insert9 = create_node_insert_change(
        "node:9",
        json!({ "id": "node:9", "other_prop": "another_value" }),
    );
    let result9 = query7.process_source_change(insert9).await;
    println!("Result 9 (Fail Missing): {:?}", result9);
    assert!(result9.is_err());
    assert!(result9.unwrap_err().to_string().contains("MissingProperty"));

    // --- Test Case 10: Size Limit - Skip ---
    println!("\n--- Test Case 10: Size Limit - Skip ---");
    let mw_name10 = "parse_skip_size";
    let query10 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name10,
            "target_prop",
            None,
            Some("skip"),
            Some(10),
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name10));
        builder.build().await
    };
    let large_json_str = r#"{"key": "this is way too long"}"#;
    let insert10 = create_node_insert_change(
        "node:10",
        json!({ "id": "node:10", "target_prop": large_json_str }),
    );
    let result10 = query10.process_source_change(insert10).await.unwrap();
    println!("Result 10 (Skip Size): {:?}", result10);
    assert_eq!(result10.len(), 1);
    assert!(result10.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:10".to_string()),
            "target_prop" => VariableValue::String(large_json_str.to_string()),
            "output_prop" => null_vv(),
            "other_prop" => null_vv()
        )
    }));

    // --- Test Case 11: Size Limit - Fail ---
    println!("\n--- Test Case 11: Size Limit - Fail ---");
    let mw_name11 = "parse_fail_size";
    let query11 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name11,
            "target_prop",
            None,
            Some("fail"),
            Some(10),
            None,
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name11));
        builder.build().await
    };
    let insert11 = create_node_insert_change(
        "node:11",
        json!({ "id": "node:11", "target_prop": large_json_str }),
    );
    let result11 = query11.process_source_change(insert11).await;
    println!("Result 11 (Fail Size): {:?}", result11);
    assert!(result11.is_err());
    assert!(result11.unwrap_err().to_string().contains("SizeExceeded"));

    // --- Test Case 12: Nesting Depth - Skip ---
    println!("\n--- Test Case 12: Nesting Depth - Skip ---");
    let mw_name12 = "parse_skip_depth";
    let query12 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name12,
            "target_prop",
            None,
            Some("skip"),
            None,
            Some(2),
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name12));
        builder.build().await
    };
    let deep_json_str = r#"{"a": {"b": {"c": "too deep"}}}"#;
    let insert12 = create_node_insert_change(
        "node:12",
        json!({ "id": "node:12", "target_prop": deep_json_str }),
    );
    let result12 = query12.process_source_change(insert12).await.unwrap();
    println!("Result 12 (Skip Depth): {:?}", result12);
    assert_eq!(result12.len(), 1);
    assert!(result12.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node:12".to_string()),
            "target_prop" => VariableValue::String(deep_json_str.to_string()),
            "output_prop" => null_vv(),
            "other_prop" => null_vv()
        )
    }));

    // --- Test Case 13: Nesting Depth - Fail ---
    println!("\n--- Test Case 13: Nesting Depth - Fail ---");
    let mw_name13 = "parse_fail_depth";
    let query13 = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::parse_json_observer_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder = builder.with_middleware_registry(middleware_registry.clone());
        let mw_config = queries::create_parse_json_middleware(
            mw_name13,
            "target_prop",
            None,
            Some("fail"),
            None,
            Some(2),
        );
        builder = builder.with_source_middleware(mw_config);
        builder = builder.with_source_pipeline("test_source", &queries::source_pipeline(mw_name13));
        builder.build().await
    };
    let insert13 = create_node_insert_change(
        "node:13",
        json!({ "id": "node:13", "target_prop": deep_json_str }),
    );
    let result13 = query13.process_source_change(insert13).await;
    println!("Result 13 (Fail Depth): {:?}", result13);
    assert!(result13.is_err());
    assert!(result13.unwrap_err().to_string().contains("DeepNesting"));
}
