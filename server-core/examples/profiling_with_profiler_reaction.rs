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

//! ProfilerReaction Example
//!
//! This example demonstrates how to use ProfilerReaction to automatically
//! collect and analyze profiling statistics across many events.
//!
//! ProfilerReaction provides:
//! - Automatic statistics calculation (mean, std dev, min, max)
//! - Percentile tracking (p50, p95, p99)
//! - Sliding window for recent samples
//! - Periodic reporting
//!
//! Run with: cargo run --example profiling_with_profiler_reaction

use drasi_server_core::{
    channels::QueryResult,
    config::ReactionConfig,
    profiling::{ProfilingMetadata, timestamp_ns},
    reactions::{ProfilerReaction, Reaction},
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    println!("=== ProfilerReaction Example ===\n");

    // STEP 1: Create ProfilerReaction configuration
    println!("Step 1: Creating ProfilerReaction configuration");

    let mut properties = std::collections::HashMap::new();
    properties.insert("window_size".to_string(), serde_json::json!(100));
    properties.insert("report_interval_secs".to_string(), serde_json::json!(10));

    let config = ReactionConfig {
        id: "profiler".to_string(),
        reaction_type: "profiler".to_string(),
        queries: vec!["example_query".to_string()],
        auto_start: true,
        properties,
    };

    println!("  window_size: 100 samples");
    println!("  log_interval: 10 seconds\n");

    // STEP 2: Create the profiler reaction
    println!("Step 2: Creating ProfilerReaction");

    let (event_tx, _event_rx) = mpsc::channel(100);
    let profiler = ProfilerReaction::new(config, event_tx);

    // Create a channel for sending query results to the profiler
    let (result_tx, result_rx) = mpsc::channel(100);

    println!("  Created ProfilerReaction and result channel\n");

    // STEP 3: Start the profiler reaction
    println!("Step 3: Starting ProfilerReaction");
    println!("  (It will log statistics every 10 seconds)\n");

    tokio::spawn(async move {
        if let Err(e) = profiler.start(result_rx).await {
            eprintln!("Error starting profiler: {}", e);
        }
    });

    // Give the profiler a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // STEP 4: Generate sample events with varying latencies
    println!("Step 4: Generating 50 sample events with profiling data");
    println!("  Simulating various latency scenarios...\n");

    for i in 0..50 {
        // Create profiling metadata with realistic timestamps
        let mut profiling = ProfilingMetadata::new();

        // Source timestamp
        profiling.source_send_ns = Some(timestamp_ns());

        // Simulate varying network latency (50-200 microseconds)
        let network_delay_us = 50 + (i * 3) % 150;
        tokio::time::sleep(tokio::time::Duration::from_micros(network_delay_us)).await;

        profiling.query_receive_ns = Some(timestamp_ns());

        // Simulate varying query processing time (100-500 microseconds)
        let query_delay_us = 100 + (i * 7) % 400;
        profiling.query_core_call_ns = Some(timestamp_ns());
        tokio::time::sleep(tokio::time::Duration::from_micros(query_delay_us)).await;
        profiling.query_core_return_ns = Some(timestamp_ns());

        profiling.query_send_ns = Some(timestamp_ns());

        // Simulate varying reaction processing time (200-800 microseconds)
        let reaction_delay_us = 200 + (i * 11) % 600;
        tokio::time::sleep(tokio::time::Duration::from_micros(reaction_delay_us / 2)).await;
        profiling.reaction_receive_ns = Some(timestamp_ns());
        tokio::time::sleep(tokio::time::Duration::from_micros(reaction_delay_us / 2)).await;
        profiling.reaction_complete_ns = Some(timestamp_ns());

        // Create query result with profiling
        let query_result = QueryResult::with_profiling(
            "example_query".to_string(),
            chrono::Utc::now(),
            vec![serde_json::json!({
                "type": "add",
                "data": {"id": format!("event_{}", i), "value": i}
            })],
            std::collections::HashMap::new(),
            profiling,
        );

        // Send to profiler
        if let Err(e) = result_tx.send(query_result).await {
            eprintln!("Error sending result to profiler: {}", e);
            break;
        }

        if (i + 1) % 10 == 0 {
            println!("  Sent {} events...", i + 1);
        }
    }

    println!("\n  All 50 events sent!");
    println!("\n=== Waiting for profiler statistics ===");
    println!("The profiler will log detailed statistics every 10 seconds.");
    println!("Statistics include:");
    println!("  - Source to Query latency");
    println!("  - Query to Reaction latency");
    println!("  - Query Core processing time");
    println!("  - Reaction processing time");
    println!("  - Total end-to-end latency");
    println!("\nFor each metric you'll see:");
    println!("  - Count, Mean, Std Dev");
    println!("  - Min, Max");
    println!("  - p50 (median), p95, p99 percentiles\n");

    // Keep the program running to see profiler output
    println!("Press Ctrl+C to exit after reviewing statistics\n");

    // Wait long enough to see at least one statistics report
    tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;

    println!("\n=== Example Complete ===");
    println!("\nKey Takeaways:");
    println!("1. ProfilerReaction automatically collects statistics");
    println!("2. Configure window_size to control sample retention");
    println!("3. Configure log_interval_seconds for reporting frequency");
    println!("4. Statistics use Welford's algorithm for efficient calculation");
    println!("5. Percentiles are calculated from a sliding window of samples");
    println!("\nIn production, use ProfilerReaction to:");
    println!("- Monitor pipeline performance");
    println!("- Identify bottlenecks (which stage has highest latency)");
    println!("- Track latency distributions over time");
    println!("- Alert on latency degradation (high p99 values)");
}
