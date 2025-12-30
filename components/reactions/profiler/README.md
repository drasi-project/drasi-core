# ProfilerReaction

A performance profiling reaction component for Drasi that collects, analyzes, and reports detailed latency metrics across the entire data processing pipeline.

## Overview

ProfilerReaction is a specialized reaction plugin that captures profiling data from query results and generates comprehensive statistical reports. It tracks end-to-end latency through the Drasi data flow pipeline, measuring:

- **Source to Query latency** - Time for data to travel from source to query component
- **Query Processing time** - Time spent executing the query logic
- **Query to Reaction latency** - Time for results to travel from query to reaction
- **Reaction Processing time** - Time spent processing results in the reaction
- **Total End-to-End latency** - Complete latency from source emission to reaction completion

### Key Capabilities

- **Real-time profiling** - Continuous collection of profiling metadata from query results
- **Statistical analysis** - Computes mean, standard deviation, variance, and percentiles (P50, P95, P99)
- **Sliding window** - Maintains a configurable window of recent samples for percentile calculations
- **Welford's algorithm** - Numerically stable online variance calculation for all samples
- **Periodic reporting** - Automatically logs comprehensive performance reports at configurable intervals
- **Multiple metrics** - Tracks five independent latency metrics simultaneously

### Use Cases

1. **Performance Monitoring** - Track system performance in production environments
2. **Bottleneck Detection** - Identify which pipeline stage is causing delays
3. **Capacity Planning** - Understand latency distributions under various loads
4. **Regression Testing** - Detect performance regressions during development
5. **SLA Validation** - Monitor P95/P99 latencies against service level objectives
6. **System Optimization** - Measure impact of configuration changes on latency

## Configuration

ProfilerReaction supports two configuration approaches:

### 1. Builder Pattern (Recommended)

The builder pattern provides a type-safe, fluent API for constructing profiler reactions:

```rust
use drasi_reaction_profiler::ProfilerReaction;

// Basic configuration
let profiler = ProfilerReaction::builder("performance-monitor")
    .with_query("user-query")
    .build()?;

// Advanced configuration
let profiler = ProfilerReaction::builder("detailed-profiler")
    .with_queries(vec!["query1".to_string(), "query2".to_string()])
    .with_window_size(2000)
    .with_report_interval_secs(30)
    .with_priority_queue_capacity(1000)
    .with_auto_start(true)
    .build()?;
```

### 2. Direct Construction

For simple use cases, direct construction with a config struct is also supported:

```rust
use drasi_reaction_profiler::{ProfilerReaction, ProfilerReactionConfig};

let config = ProfilerReactionConfig {
    window_size: 1000,
    report_interval_secs: 60,
};

let profiler = ProfilerReaction::new(
    "my-profiler",
    vec!["query1".to_string()],
    config
);
```

## Configuration Options

| Option | Description | Data Type | Valid Values | Default |
|--------|-------------|-----------|--------------|---------|
| `id` | Unique identifier for the profiler reaction | `String` | Any non-empty string | Required |
| `queries` | List of query IDs to subscribe to for profiling | `Vec<String>` | Valid query IDs | `[]` (empty) |
| `window_size` | Number of recent samples to retain for percentile calculations | `usize` | Positive integer | `1000` |
| `report_interval_secs` | Interval in seconds between automatic report generation | `u64` | Positive integer | `60` |
| `priority_queue_capacity` | Capacity of the internal priority queue for query results | `Option<usize>` | Positive integer or `None` | `None` (unlimited) |
| `auto_start` | Whether to automatically start the reaction when added | `bool` | `true` or `false` | `true` |

### Configuration Details

**window_size**: Controls the sliding window for percentile calculations. Larger windows provide more stable percentiles but consume more memory. The window only affects percentile calculations (min, max, P50, P95, P99); mean and variance are computed across all samples using Welford's algorithm.

**report_interval_secs**: Determines how frequently profiling reports are logged. Shorter intervals provide more frequent feedback but generate more log output. Set to a higher value (e.g., 300) for production systems.

**priority_queue_capacity**: Limits memory usage by bounding the internal queue. When `None`, the queue can grow unbounded. Recommended to set explicitly in production environments based on expected throughput.

## Output Schema

ProfilerReaction logs reports to the standard logging system (typically via `RUST_LOG` configuration). Each report includes five metric sections with the following format:

```
[profiler-id] ========== Profiling Report ==========
[profiler-id] Source→Query: mean=X.XXms, stddev=X.XXms, min=X.XXms, p50=X.XXms, p95=X.XXms, p99=X.XXms, max=X.XXms (n=XXXX)
[profiler-id] Query Processing: mean=X.XXms, stddev=X.XXms, min=X.XXms, p50=X.XXms, p95=X.XXms, p99=X.XXms, max=X.XXms (n=XXXX)
[profiler-id] Query→Reaction: mean=X.XXms, stddev=X.XXms, min=X.XXms, p50=X.XXms, p95=X.XXms, p99=X.XXms, max=X.XXms (n=XXXX)
[profiler-id] Reaction Processing: mean=X.XXms, stddev=X.XXms, min=X.XXms, p50=X.XXms, p95=X.XXms, p99=X.XXms, max=X.XXms (n=XXXX)
[profiler-id] Total End-to-End: mean=X.XXms, stddev=X.XXms, min=X.XXms, p50=X.XXms, p95=X.XXms, p99=X.XXms, max=X.XXms (n=XXXX)
[profiler-id] ======================================
```

### Metric Fields

- **mean**: Average latency across all samples (Welford's algorithm)
- **stddev**: Standard deviation, measuring variability in latencies
- **min**: Minimum latency in the current window
- **max**: Maximum latency in the current window
- **p50**: 50th percentile (median) latency in the current window
- **p95**: 95th percentile latency in the current window
- **p99**: 99th percentile latency in the current window
- **n**: Total number of samples collected since startup

All latency values are reported in milliseconds with two decimal places. Internally, measurements are captured in nanoseconds for precision.

## Usage Examples

### Example 1: Basic Performance Monitoring

Monitor a single query with default settings:

```rust
use drasi_reaction_profiler::ProfilerReaction;
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create profiler for a query
    let profiler = ProfilerReaction::builder("query-profiler")
        .with_query("temperature-alerts")
        .build()?;

    // Add to DrasiLib
    let mut drasi = DrasiLib::new("monitoring-system").build().await?;
    drasi.add_reaction(profiler).await?;
    drasi.start().await?;

    Ok(())
}
```

### Example 2: High-Frequency Profiling

Profile with a smaller window and more frequent reports for detailed analysis:

```rust
let profiler = ProfilerReaction::builder("high-freq-profiler")
    .with_query("real-time-events")
    .with_window_size(500)           // Smaller window
    .with_report_interval_secs(10)   // Report every 10 seconds
    .build()?;
```

### Example 3: Multi-Query Profiling

Profile multiple queries simultaneously:

```rust
let profiler = ProfilerReaction::builder("multi-query-profiler")
    .with_queries(vec![
        "sensor-data".to_string(),
        "alert-triggers".to_string(),
        "aggregations".to_string(),
    ])
    .with_window_size(2000)
    .with_report_interval_secs(60)
    .build()?;
```

### Example 4: Production Configuration

Recommended configuration for production environments:

```rust
let profiler = ProfilerReaction::builder("production-profiler")
    .with_query("critical-path")
    .with_window_size(10000)              // Larger window for stable percentiles
    .with_report_interval_secs(300)       // Report every 5 minutes
    .with_priority_queue_capacity(5000)   // Bounded queue to limit memory
    .build()?;
```

### Example 5: Manual Start/Stop Control

Create a profiler that doesn't auto-start for controlled profiling sessions:

```rust
use drasi_lib::Reaction;

let profiler = ProfilerReaction::builder("controlled-profiler")
    .with_query("test-query")
    .with_auto_start(false)
    .build()?;

// Add to system
drasi.add_reaction(profiler.clone()).await?;

// Start profiling when needed
profiler.start().await?;

// ... collect data ...

// Stop profiling
profiler.stop().await?;
```

### Example 6: Viewing Logs

To see profiling reports, configure Rust logging appropriately:

```bash
# Enable INFO level logs for the profiler
RUST_LOG=drasi_reaction_profiler=info cargo run

# Or enable all info logs
RUST_LOG=info cargo run

# For debugging internal profiler behavior
RUST_LOG=drasi_reaction_profiler=debug cargo run
```

Example output:

```
[INFO] [performance-monitor] Profiler started - window_size: 1000, report_interval: 60s
[INFO] [performance-monitor] ========== Profiling Report ==========
[INFO] [performance-monitor] Source→Query: mean=2.45ms, stddev=0.83ms, min=1.20ms, p50=2.40ms, p95=4.10ms, p99=5.20ms, max=6.80ms (n=5432)
[INFO] [performance-monitor] Query Processing: mean=15.32ms, stddev=4.21ms, min=8.50ms, p50=14.80ms, p95=23.40ms, p99=28.90ms, max=35.20ms (n=5432)
[INFO] [performance-monitor] Query→Reaction: mean=0.85ms, stddev=0.22ms, min=0.45ms, p50=0.82ms, p95=1.25ms, p99=1.48ms, max=2.10ms (n=5432)
[INFO] [performance-monitor] Reaction Processing: mean=3.12ms, stddev=1.05ms, min=1.80ms, p50=3.00ms, p95=5.20ms, p99=6.40ms, max=8.50ms (n=5432)
[INFO] [performance-monitor] Total End-to-End: mean=21.74ms, stddev=5.15ms, min=12.50ms, p50=21.20ms, p95=31.80ms, p99=38.50ms, max=48.20ms (n=5432)
[INFO] [performance-monitor] ======================================
```

## Implementation Details

### Statistical Algorithms

**Welford's Algorithm**: ProfilerReaction uses Welford's online algorithm for computing mean and variance. This approach provides:
- O(1) memory usage regardless of sample count
- Numerical stability even with large values
- Incremental updates without storing all samples

**Percentile Calculation**: Percentiles (min, max, P50, P95, P99) are computed from a sliding window of recent samples:
- Window size is configurable via `window_size` parameter
- Older samples are discarded when window is full
- Percentiles reflect recent behavior, while mean/variance reflect all-time behavior

### Profiling Metadata

ProfilerReaction relies on profiling metadata embedded in query results. The metadata includes nanosecond timestamps for:
- `source_send_ns` - When source emitted the data
- `query_receive_ns` - When query received the data
- `query_core_call_ns` - When query processing began
- `query_core_return_ns` - When query processing completed
- `query_send_ns` - When query sent the result
- `reaction_receive_ns` - When reaction received the result
- `reaction_complete_ns` - When reaction finished processing

Missing timestamps are handled gracefully; metrics requiring those timestamps will not be updated.

### Thread Safety

ProfilerReaction is fully async and thread-safe:
- Uses `Arc<RwLock<T>>` for shared state
- Async processing loop runs in a Tokio task
- All public methods are async and can be called from multiple tasks

### Memory Management

- **Fixed window**: Only `window_size` samples are retained for percentiles
- **Bounded queue**: Optional `priority_queue_capacity` limits memory usage
- **Efficient storage**: Mean/variance use constant memory via Welford's algorithm

## Integration with DrasiLib

ProfilerReaction implements the `Reaction` trait from drasi-lib and integrates seamlessly:

```rust
// Create and add to DrasiLib
let profiler = ProfilerReaction::builder("profiler").build()?;
drasi.add_reaction(profiler).await?;

// Query subscriber is automatically injected
// Start/stop lifecycle managed through Reaction trait
```

The reaction automatically subscribes to configured queries when started and processes profiling metadata from query results.

## Dependencies

ProfilerReaction requires the following Rust crates:

- `drasi-lib` - Core Drasi library (Reaction trait, profiling metadata)
- `tokio` - Async runtime for concurrent processing
- `log` - Logging macros for report output
- `serde` / `serde_json` - Configuration serialization
- `anyhow` - Error handling
- `async-trait` - Async trait support

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
