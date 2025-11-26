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

//! ProfilerReaction Configuration Example
//!
//! This example demonstrates how to configure ProfilerReaction to automatically
//! collect and analyze profiling statistics across many events.
//!
//! ProfilerReaction provides:
//! - Automatic statistics calculation (mean, std dev, min, max)
//! - Percentile tracking (p50, p95, p99)
//! - Sliding window for recent samples
//! - Periodic reporting
//!
//! Run with: cargo run --example profiling_with_profiler_reaction

fn main() {
    println!("=== ProfilerReaction Configuration Example ===\n");

    // STEP 1: Show ProfilerReaction configuration
    println!("Step 1: ProfilerReaction YAML configuration");

    let config = r#"
server_core:
  id: profiler-example

sources:
  - id: sensor_source
    source_type: mock
    auto_start: true
    properties:
      data_type: sensor
      interval_ms: 1000

queries:
  - id: example_query
    query: "MATCH (n:Sensor) RETURN n"
    sources:
      - sensor_source
    auto_start: true

reactions:
  - id: profiler
    reaction_type: profiler
    queries:
      - example_query
    auto_start: true
    properties:
      window_size: 100
      report_interval_secs: 10
"#;

    println!("{}", config);
    println!("Configuration notes:");
    println!("  - window_size: 100 samples (sliding window for percentiles)");
    println!("  - report_interval_secs: 10 (log statistics every 10 seconds)\n");

    // STEP 2: Explain what ProfilerReaction tracks
    println!("Step 2: What ProfilerReaction tracks");
    println!("  - Source to Query latency");
    println!("  - Query to Reaction latency");
    println!("  - Query Core processing time");
    println!("  - Reaction processing time");
    println!("  - Total end-to-end latency\n");

    // STEP 3: Show sample output
    println!("Step 3: Sample ProfilerReaction output");
    println!("  When running, the profiler logs statistics like:");
    println!();
    println!("  [ProfilerReaction] Statistics for query 'example_query':");
    println!("    Source→Query:    count=100, mean=1.2ms, std=0.3ms, min=0.5ms, max=2.1ms");
    println!("                     p50=1.1ms, p95=1.8ms, p99=2.0ms");
    println!("    Query→Reaction:  count=100, mean=0.8ms, std=0.2ms, min=0.4ms, max=1.5ms");
    println!("                     p50=0.7ms, p95=1.2ms, p99=1.4ms");
    println!("    Query Core:      count=100, mean=0.5ms, std=0.1ms, min=0.3ms, max=0.9ms");
    println!("                     p50=0.5ms, p95=0.7ms, p99=0.8ms");
    println!("    Total E2E:       count=100, mean=2.5ms, std=0.5ms, min=1.2ms, max=4.5ms");
    println!("                     p50=2.3ms, p95=3.5ms, p99=4.2ms\n");

    // STEP 4: Usage notes
    println!("Step 4: Using ProfilerReaction");
    println!("  1. Add the profiler reaction to your config file");
    println!("  2. Subscribe it to the queries you want to monitor");
    println!("  3. Set appropriate window_size and report_interval_secs");
    println!("  4. Run your server and check the logs for statistics\n");

    println!("=== Key Takeaways ===");
    println!("1. ProfilerReaction automatically collects statistics");
    println!("2. Configure window_size to control sample retention");
    println!("3. Configure report_interval_secs for reporting frequency");
    println!("4. Statistics use Welford's algorithm for efficient calculation");
    println!("5. Percentiles are calculated from a sliding window of samples\n");

    println!("In production, use ProfilerReaction to:");
    println!("- Monitor pipeline performance");
    println!("- Identify bottlenecks (which stage has highest latency)");
    println!("- Track latency distributions over time");
    println!("- Alert on latency degradation (high p99 values)");
}
