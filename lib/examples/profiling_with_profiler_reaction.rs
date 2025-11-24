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

use drasi_lib::{
    config::{ReactionConfig, ReactionSpecificConfig},
    reactions::profiler::ProfilerReactionConfig,
    reactions::{ProfilerReaction, Reaction},
    server_core::DrasiServerCore,
};
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    println!("=== ProfilerReaction Example ===\n");

    // STEP 1: Create ProfilerReaction configuration
    println!("Step 1: Creating ProfilerReaction configuration");

    let config = ReactionConfig {
        id: "profiler".to_string(),
        queries: vec!["example_query".to_string()],
        auto_start: true,
        config: ReactionSpecificConfig::Profiler(ProfilerReactionConfig {
            window_size: 100,
            report_interval_secs: 10,
        }),
        priority_queue_capacity: None,
    };

    println!("  window_size: 100 samples");
    println!("  log_interval: 10 seconds\n");

    // STEP 2: Create the profiler reaction
    println!("Step 2: Creating ProfilerReaction");

    let (event_tx, _event_rx) = mpsc::channel(100);
    let profiler = ProfilerReaction::new(config, event_tx);

    // Create a DrasiServerCore with a mock query
    let server_core = DrasiServerCore::builder()
        .build()
        .await
        .expect("Failed to create server core");

    // Create the mock query with ID matching the one in profiler config
    // For simplicity, we'll just use the server core without actually creating a query
    // The profiler will try to subscribe but since there's no query, it won't receive anything
    // So we'll send results manually using a direct channel

    println!("  Created ProfilerReaction and server core\n");

    // STEP 3: Start the profiler reaction
    println!("Step 3: Starting ProfilerReaction");
    println!("  (It will log statistics every 10 seconds)\n");
    println!("  Note: This is a simplified example. In production, reactions subscribe to real queries.\n");

    let server_core_arc = Arc::new(server_core);
    tokio::spawn(async move {
        if let Err(e) = profiler.start(server_core_arc).await {
            eprintln!("Error starting profiler: {}", e);
        }
    });

    // Give the profiler a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // STEP 4: Note about sending events
    println!("Step 4: In a real scenario, events would come from queries");
    println!("  For this simplified example, the profiler is running but won't receive events");
    println!("  since we haven't set up a complete query pipeline.\n");
    println!("  See the integration tests for complete examples.\n");

    // Keep the program running for a bit to show the profiler is active
    println!("Profiler is running. Press Ctrl+C to exit.\n");

    // Simulate some passage of time to show profiler is running
    for i in 0..5 {
        println!("  Tick {}... (Profiler running in background)", i + 1);
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    println!("\n=== What ProfilerReaction Would Show ===");
    println!("In a real scenario with a complete query pipeline, the profiler logs:");
    println!("Statistics every 10 seconds including:");
    println!("  - Source to Query latency");
    println!("  - Query to Reaction latency");
    println!("  - Query Core processing time");
    println!("  - Reaction processing time");
    println!("  - Total end-to-end latency");
    println!("\nFor each metric you'll see:");
    println!("  - Count, Mean, Std Dev");
    println!("  - Min, Max");
    println!("  - p50 (median), p95, p99 percentiles\n");

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
