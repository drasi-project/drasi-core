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

//! Tests simulating processing data from a Dapr-like state store source.
//! Data flow: Quoted Base64 JSON -> Decode -> Parse JSON -> Promote fields.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as base64_engine, Engine as _};
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
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_middleware::{
    decoder::DecoderFactory, parse_json::ParseJsonFactory, promote::PromoteMiddlewareFactory,
};
use drasi_query_cypher::CypherParser;
use serde_json::{json, Value as JsonValue};

use crate::QueryTestConfig;

mod queries;

// --- Constants ---
const SOURCE_ID: &str = "dapr_source";
const ELEMENT_LABEL: &str = "DaprState";
const RAW_DATA_PROP: &str = "rawData"; // Quoted, base64 encoded JSON
const PARSED_DATA_PROP: &str = "parsedData"; // Parsed JSON structure

// --- Helper Macros & Functions ---

macro_rules! variablemap {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::BTreeMap::new();
         $( map.insert($key.to_string().into(), $val); )*
         map
    }}
}

fn null_vv() -> VariableValue {
    VariableValue::Null
}

fn create_middleware_registry() -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(DecoderFactory::new()));
    registry.register(Arc::new(ParseJsonFactory::new()));
    registry.register(Arc::new(PromoteMiddlewareFactory::new()));
    Arc::new(registry)
}

/// Encodes JSON, wraps in quotes, and creates an ElementValue::String.
fn create_dapr_value(data: &JsonValue) -> ElementValue {
    let json_string = data.to_string();
    let base64_encoded = base64_engine.encode(json_string.as_bytes());
    let quoted_base64 = format!("\"{base64_encoded}\"");
    ElementValue::String(Arc::from(quoted_base64.as_str()))
}

/// Creates a SourceChange::Insert for a Dapr element.
fn create_dapr_insert(element_id: &str, timestamp: u64, data: &JsonValue) -> SourceChange {
    let mut props = ElementPropertyMap::new();
    props.insert(RAW_DATA_PROP, create_dapr_value(data));

    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, element_id),
                labels: Arc::new([Arc::from(ELEMENT_LABEL)]),
                effective_from: timestamp,
            },
            properties: props,
        },
    }
}

/// Creates a SourceChange::Update for a Dapr element.
fn create_dapr_update(element_id: &str, timestamp: u64, data: &JsonValue) -> SourceChange {
    let mut props = ElementPropertyMap::new();
    props.insert(RAW_DATA_PROP, create_dapr_value(data));

    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, element_id),
                labels: Arc::new([Arc::from(ELEMENT_LABEL)]),
                effective_from: timestamp,
            },
            properties: props,
        },
    }
}

/// Creates a SourceChange::Delete for a Dapr element.
fn create_dapr_delete(element_id: &str, timestamp: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(SOURCE_ID, element_id),
            labels: Arc::new([Arc::from(ELEMENT_LABEL)]),
            effective_from: timestamp,
        },
    }
}

/// Sets up the ContinuousQuery with the pipeline.
async fn setup_query(
    config: &(impl QueryTestConfig + Send),
    test_name: &str,
    promote_mappings: JsonValue,
    decoder_on_error: Option<&str>,
    parser_on_error: Option<&str>,
    promoter_on_conflict: Option<&str>,
    promoter_on_error: Option<&str>,
) -> ContinuousQuery {
    let registry = create_middleware_registry();
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let mut builder = QueryBuilder::new(queries::observer_query(), parser)
        .with_function_registry(function_registry);
    builder = config.config_query(builder).await;
    builder = builder.with_middleware_registry(registry);

    // Create unique names for middleware instances per test
    let decoder_mw_name = format!("{test_name}_decoder");
    let parser_mw_name = format!("{test_name}_parser");
    let promoter_mw_name = format!("{test_name}_promoter");

    // Add middleware configs using helpers from queries.rs
    builder = builder.with_source_middleware(queries::create_decoder_config(
        &decoder_mw_name,
        decoder_on_error,
    ));
    builder = builder.with_source_middleware(queries::create_parser_config(
        &parser_mw_name,
        parser_on_error,
    ));
    builder = builder.with_source_middleware(queries::create_promoter_config(
        &promoter_mw_name,
        promote_mappings,
        promoter_on_conflict,
        promoter_on_error,
    ));

    // Define pipeline using helpers from queries.rs
    let pipeline_steps =
        queries::create_pipeline(&decoder_mw_name, &parser_mw_name, &promoter_mw_name);
    builder = builder.with_source_pipeline(SOURCE_ID, &pipeline_steps);

    builder.build().await
}

// --- Test Cases ---

#[allow(clippy::unwrap_used)]
async fn test_insert_basic_flow(config: &(impl QueryTestConfig + Send)) {
    let test_name = "insert_basic";
    let element_id = "state-001";
    let timestamp = 100;
    let input_json = json!({
        "user": { "id": "u-123", "name": "Alice" },
        "order": { "id": "o-456", "total": 99.50 },
        "status": "completed"
    });
    let promote_mappings = json!([
        { "path": format!("$.{}.user.id", PARSED_DATA_PROP), "target_name": "userId" },
        { "path": format!("$.{}.order.total", PARSED_DATA_PROP), "target_name": "orderTotal" },
        { "path": format!("$.{}.status", PARSED_DATA_PROP), "target_name": "orderStatus" }
    ]);

    let query = setup_query(
        config,
        test_name,
        promote_mappings,
        Some("fail"),
        Some("fail"),
        None,
        Some("fail"),
    )
    .await;

    let insert_change = create_dapr_insert(element_id, timestamp, &input_json);
    let result = query.process_source_change(insert_change).await.unwrap();

    assert_eq!(result.len(), 1, "Expected one result context");

    // Expected state after middleware pipeline
    let expected_vars = variablemap!(
        RAW_DATA_PROP => VariableValue::String(input_json.to_string()),
        PARSED_DATA_PROP => VariableValue::from(input_json.clone()),
        "userId" => VariableValue::String("u-123".to_string()),
        "orderTotal" => VariableValue::Float(Float::from(99.50)),
        "orderStatus" => VariableValue::String("completed".to_string()),
        "promotedValue" => null_vv(),
        "promotedTag" => null_vv()
    );

    assert!(
        result.contains(&QueryPartEvaluationContext::Adding {
            after: expected_vars
        }),
        "Result did not contain expected Adding context. Got: {result:?}"
    );
}

#[allow(clippy::unwrap_used)]
async fn test_update_basic_flow(config: &(impl QueryTestConfig + Send)) {
    let test_name = "update_basic";
    let element_id = "state-002";
    let ts1 = 100;
    let ts2 = 110;
    let input_json_v1 = json!({ "value": 10, "tag": "initial" });
    let input_json_v2 = json!({ "value": 20, "tag": "updated" });

    let promote_mappings = json!([
        { "path": format!("$.{}.value", PARSED_DATA_PROP), "target_name": "promotedValue" },
        { "path": format!("$.{}.tag", PARSED_DATA_PROP), "target_name": "promotedTag" }
    ]);

    let query = setup_query(
        config,
        test_name,
        promote_mappings,
        Some("fail"),
        Some("fail"),
        None,
        Some("fail"),
    )
    .await;

    // Insert initial version
    let insert_change = create_dapr_insert(element_id, ts1, &input_json_v1);
    let insert_result = query.process_source_change(insert_change).await.unwrap();
    assert_eq!(insert_result.len(), 1);

    // Update
    let update_change = create_dapr_update(element_id, ts2, &input_json_v2);
    let update_result = query.process_source_change(update_change).await.unwrap();
    assert_eq!(
        update_result.len(),
        1,
        "Expected one result context for update"
    );

    let expected_vars_before = variablemap!(
        RAW_DATA_PROP => VariableValue::String(input_json_v1.to_string()),
        PARSED_DATA_PROP => VariableValue::from(input_json_v1.clone()),
        "promotedValue" => VariableValue::Integer(Integer::from(10)),
        "promotedTag" => VariableValue::String("initial".to_string()),
        "userId" => null_vv(),
        "orderTotal" => null_vv(),
        "orderStatus" => null_vv()
    );

    let expected_vars_after = variablemap!(
        RAW_DATA_PROP => VariableValue::String(input_json_v2.to_string()),
        PARSED_DATA_PROP => VariableValue::from(input_json_v2.clone()),
        "promotedValue" => VariableValue::Integer(Integer::from(20)),
        "promotedTag" => VariableValue::String("updated".to_string()),
        "userId" => null_vv(),
        "orderTotal" => null_vv(),
        "orderStatus" => null_vv()
    );

    assert!(
        update_result.contains(&QueryPartEvaluationContext::Updating {
            before: expected_vars_before,
            after: expected_vars_after
        }),
        "Result did not contain expected Updating context. Got: {update_result:?}"
    );
}

#[allow(clippy::unwrap_used)]
async fn test_delete_passthrough(config: &(impl QueryTestConfig + Send)) {
    let test_name = "delete_passthrough";
    let element_id = "state-003";
    let ts1 = 100;
    let ts2 = 110;
    let input_json_v1 = json!({ "value": 50 });

    let promote_mappings = json!([
        { "path": format!("$.{}.value", PARSED_DATA_PROP), "target_name": "promotedValue" }
    ]);

    let query = setup_query(
        config,
        test_name,
        promote_mappings,
        Some("fail"),
        Some("fail"),
        None,
        Some("fail"),
    )
    .await;

    // Insert initial version
    let insert_change = create_dapr_insert(element_id, ts1, &input_json_v1);
    let insert_result = query.process_source_change(insert_change).await.unwrap();
    assert_eq!(insert_result.len(), 1);

    let expected_vars_before_delete = variablemap!(
        RAW_DATA_PROP => VariableValue::String(input_json_v1.to_string()),
        PARSED_DATA_PROP => VariableValue::from(input_json_v1.clone()),
        "promotedValue" => VariableValue::Integer(Integer::from(50)),
        "userId" => null_vv(),
        "orderTotal" => null_vv(),
        "orderStatus" => null_vv(),
        "promotedTag" => null_vv()
    );

    // Delete
    let delete_change = create_dapr_delete(element_id, ts2);
    let delete_result = query.process_source_change(delete_change).await.unwrap();
    assert_eq!(
        delete_result.len(),
        1,
        "Expected one result context for delete"
    );

    assert!(
        delete_result.contains(&QueryPartEvaluationContext::Removing {
            before: expected_vars_before_delete
        }),
        "Result did not contain expected Removing context. Got: {delete_result:?}"
    );
}

// --- Test Suite Runner ---

/// Runs all tests in this module.
pub async fn run_tests(config: &(impl QueryTestConfig + Send)) {
    log::info!("Running Dapr State Store tests...");
    test_insert_basic_flow(config).await;
    test_update_basic_flow(config).await;
    test_delete_passthrough(config).await;
    log::info!("Dapr State Store tests completed.");
}
