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

//! Example demonstrating Query configuration with middleware
//!
//! This example shows how to configure queries with middleware using YAML configuration.
//! Middleware allows you to transform data as it flows through the pipeline.
//!
//! In the plugin architecture:
//! - Queries are defined in YAML configuration files
//! - Middleware is configured per-query with source pipelines
//! - Each middleware has a kind, name, and configuration

fn main() {
    println!("=== Query Middleware Configuration Example ===\n");

    // Example 1: Simple middleware configuration
    println!("Example 1: Simple middleware configuration");
    let config1 = r#"
queries:
  - id: query1
    query: "MATCH (n:Sensor) RETURN n"
    sources:
      - sensor_data
    middleware:
      - kind: jq
        name: temperature_converter
        config:
          filter: ".temperature | . * 9 / 5 + 32"
    source_pipelines:
      sensor_data:
        - temperature_converter
"#;
    println!("{}", config1);
    println!("  This query converts temperature from Celsius to Fahrenheit\n");

    // Example 2: Multiple middleware in a pipeline
    println!("Example 2: Multiple middleware in a pipeline");
    let config2 = r#"
queries:
  - id: query2
    query: "MATCH (n:Event) RETURN n"
    sources:
      - raw_events
    middleware:
      - kind: jq
        name: json_decoder
        config:
          filter: ".payload | fromjson"
      - kind: jq
        name: value_validator
        config:
          filter: "select(.value > 0 and .value < 1000)"
      - kind: jq
        name: timestamp_formatter
        config:
          filter: ".timestamp | strftime(\"%Y-%m-%d %H:%M:%S\")"
    source_pipelines:
      raw_events:
        - json_decoder
        - value_validator
        - timestamp_formatter
"#;
    println!("{}", config2);
    println!("  This query applies 3 transformations in sequence\n");

    // Example 3: Multiple sources with different pipelines
    println!("Example 3: Multiple sources with different pipelines");
    let config3 = r#"
queries:
  - id: query3
    query: "MATCH (n) RETURN n"
    sources:
      - raw_source
      - clean_source
      - trusted_source
    middleware:
      - kind: jq
        name: decoder
        config:
          filter: ".data | fromjson"
      - kind: jq
        name: validator
        config:
          filter: "select(.status == \"active\")"
    source_pipelines:
      raw_source:
        - decoder
        - validator
      clean_source:
        - validator
      trusted_source: []
"#;
    println!("{}", config3);
    println!("  Different sources get different middleware pipelines\n");

    // Example 4: Query with other configuration options
    println!("Example 4: Query with other configuration options");
    let config4 = r#"
queries:
  - id: query4
    query: "MATCH (n:Stock) WHERE n.price > 100 RETURN n"
    sources:
      - stock_feed
    auto_start: false
    priority_queue_capacity: 50000
    middleware:
      - kind: jq
        name: price_filter
        config:
          filter: "select(.price > 0)"
    source_pipelines:
      stock_feed:
        - price_filter
"#;
    println!("{}", config4);
    println!("  Query with auto_start=false and custom capacity\n");

    println!("=== Key Takeaways ===");
    println!("1. Middleware is defined in the 'middleware' array of a query config");
    println!("2. Each middleware has: kind, name, and config");
    println!("3. Source pipelines map sources to ordered lists of middleware names");
    println!("4. Middleware executes in the order defined in the pipeline");
    println!("5. Different sources can use different middleware pipelines");
}
