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

//! Example demonstrating the Query Builder API for middleware configuration
//!
//! This example shows how to:
//! 1. Add middleware configurations using `with_middleware()`
//! 2. Configure source pipelines using `with_source_pipeline()`
//! 3. Chain these methods with other builder methods

use drasi_core::models::SourceMiddlewareConfig;
use drasi_server_core::Query;
use std::sync::Arc;

fn main() {
    // Example 1: Simple middleware configuration
    println!("Example 1: Simple middleware configuration");
    let _query1 = Query::cypher("query1")
        .query("MATCH (n:Sensor) RETURN n")
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("temperature_converter"),
            config: serde_json::json!({
                "filter": ".temperature | . * 9 / 5 + 32"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_source_pipeline("sensor_data", vec!["temperature_converter".to_string()])
        .build();
    println!("  Created query with temperature converter middleware");

    // Example 2: Multiple middleware in a pipeline
    println!("\nExample 2: Multiple middleware in a pipeline");
    let _query2 = Query::cypher("query2")
        .query("MATCH (n:Event) RETURN n")
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("json_decoder"),
            config: serde_json::json!({
                "filter": ".payload | fromjson"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("value_validator"),
            config: serde_json::json!({
                "filter": "select(.value > 0 and .value < 1000)"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("timestamp_formatter"),
            config: serde_json::json!({
                "filter": ".timestamp | strftime(\"%Y-%m-%d %H:%M:%S\")"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_source_pipeline(
            "raw_events",
            vec![
                "json_decoder".to_string(),
                "value_validator".to_string(),
                "timestamp_formatter".to_string(),
            ],
        )
        .build();
    println!("  Created query with 3-stage middleware pipeline");

    // Example 3: Multiple sources with different pipelines
    println!("\nExample 3: Multiple sources with different pipelines");
    let _query3 = Query::cypher("query3")
        .query("MATCH (n) RETURN n")
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("decoder"),
            config: serde_json::json!({
                "filter": ".data | fromjson"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("validator"),
            config: serde_json::json!({
                "filter": "select(.status == \"active\")"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        // Raw source needs both decoding and validation
        .with_source_pipeline(
            "raw_source",
            vec!["decoder".to_string(), "validator".to_string()],
        )
        // Clean source only needs validation
        .with_source_pipeline("clean_source", vec!["validator".to_string()])
        // Trusted source needs no middleware
        .with_source_pipeline("trusted_source", vec![])
        .build();
    println!("  Created query with 3 sources using different pipelines");

    // Example 4: Method chaining with other builder methods
    println!("\nExample 4: Method chaining with other builder methods");
    let _query4 = Query::cypher("query4")
        .query("MATCH (n:Stock) WHERE n.price > 100 RETURN n")
        .auto_start(false)
        .with_priority_queue_capacity(50000)
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("price_filter"),
            config: serde_json::json!({
                "filter": "select(.price > 0)"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_source_pipeline("stock_feed", vec!["price_filter".to_string()])
        .build();
    println!("  Created query with middleware, auto_start=false, and custom capacity");

    // Example 5: Middleware without pipeline (available but not used)
    println!("\nExample 5: Middleware without pipeline");
    let _query5 = Query::cypher("query5")
        .query("MATCH (n) RETURN n")
        .with_middleware(SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("optional_transform"),
            config: serde_json::json!({
                "filter": ".value | tonumber"
            })
            .as_object()
            .unwrap()
            .clone(),
        })
        .with_source_pipeline("source1", vec![]) // Source with no pipeline
        .build();
    println!("  Created query with middleware defined but not used in pipeline");

    println!("\nAll examples completed successfully!");
}
