# DrasiServerCore Profiling Guide

This guide provides comprehensive documentation for profiling end-to-end latency in DrasiServerCore applications.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Quick Start](#quick-start)
- [Profiling Timestamps](#profiling-timestamps)
- [ProfilerReaction](#profilerreaction)
- [Manual Profiling](#manual-profiling)
- [Source Configuration](#source-configuration)
- [Statistical Methods](#statistical-methods)
- [Performance Impact](#performance-impact)
- [Best Practices](#best-practices)
- [Troubleshooting](#troubleshooting)
- [Examples](#examples)

## Overview

DrasiServerCore's profiling system tracks end-to-end latency through the entire data pipeline:

```
Source → Query → Reaction
   |       |        |
   └───────┴────────┴─── Profiling captures timestamps at each stage
```

**Key Features:**

- **9 Timestamp Capture Points**: Track latency at every stage
- **Automatic Statistics**: Mean, std dev, min, max, percentiles (p50, p95, p99)
- **Zero-Copy Design**: Profiling metadata flows through existing structures
- **Optional**: Enable only when needed, no performance cost when disabled
- **Nanosecond Precision**: Accurate measurements using `std::time::Instant`

**Use Cases:**

- Performance monitoring and optimization
- Bottleneck identification
- SLA/SLO compliance tracking
- Latency regression detection
- Capacity planning

## Architecture

### Data Flow

```
┌──────────┐
│  Source  │  1. source_ns (data generated/received)
│          │  2. source_receive_ns (external event received)
│          │  3. source_send_ns (sent to query)
└────┬─────┘
     │ ProfilingMetadata flows through SourceEventWrapper
     ▼
┌──────────┐
│  Query   │  4. query_receive_ns (received from source)
│          │  5. query_core_call_ns (before core evaluation)
│          │  6. query_core_return_ns (after core evaluation)
│          │  7. query_send_ns (sent to reaction)
└────┬─────┘
     │ ProfilingMetadata flows through QueryResult
     ▼
┌──────────┐
│ Reaction │  8. reaction_receive_ns (received from query)
│          │  9. reaction_complete_ns (processing complete)
└──────────┘
```

### Profiling Metadata Structure

```rust
pub struct ProfilingMetadata {
    // Source timestamps
    pub source_ns: Option<u64>,
    pub source_receive_ns: Option<u64>,
    pub source_send_ns: Option<u64>,

    // Query timestamps
    pub query_receive_ns: Option<u64>,
    pub query_core_call_ns: Option<u64>,
    pub query_core_return_ns: Option<u64>,
    pub query_send_ns: Option<u64>,

    // Reaction timestamps
    pub reaction_receive_ns: Option<u64>,
    pub reaction_complete_ns: Option<u64>,
}
```

All timestamps are nanoseconds since an arbitrary epoch (typically system boot).

## Quick Start

### 1. Enable Profiling in Source

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};

let core = DrasiServerCore::builder()
    .add_source(
        Source::mock("profiled-source")
            .with_properties(
                Properties::new()
                    .with_bool("enable_profiling", true)
            )
            .build()
    )
    .add_query(
        Query::cypher("my-query")
            .query("MATCH (n:Node) RETURN n")
            .from_source("profiled-source")
            .build()
    )
    .add_reaction(
        Reaction::profiler("profiler")
            .subscribe_to("my-query")
            .with_properties(
                Properties::new()
                    .with_int("window_size", 1000)
                    .with_int("log_interval_seconds", 30)
            )
            .build()
    )
    .build()
    .await?;

core.start().await?;
```

### 2. View Statistics

ProfilerReaction automatically logs statistics every 30 seconds (configurable):

```
========================================
Profiling Statistics (1000 samples, window: 1000)
========================================

Source → Query Latency:
  count: 1000, mean: 245.3 μs, std dev: 78.2 μs
  min: 120.5 μs, max: 892.1 μs
  p50: 235.0 μs, p95: 387.5 μs, p99: 524.8 μs

Query → Reaction Latency:
  count: 1000, mean: 156.8 μs, std dev: 42.1 μs
  min: 95.3 μs, max: 445.2 μs
  p50: 148.5 μs, p95: 225.7 μs, p99: 312.4 μs

Query Core Processing Time:
  count: 1000, mean: 1.2 ms, std dev: 0.4 ms
  min: 0.5 ms, max: 3.8 ms
  p50: 1.1 ms, p95: 1.9 ms, p99: 2.4 ms

Reaction Processing Time:
  count: 1000, mean: 850.5 μs, std dev: 210.3 μs
  min: 400.2 μs, max: 2.1 ms
  p50: 820.0 μs, p95: 1.3 ms, p99: 1.7 ms

Total End-to-End Latency:
  count: 1000, mean: 2.5 ms, std dev: 0.6 ms
  min: 1.2 ms, max: 5.3 ms
  p50: 2.4 ms, p95: 3.6 ms, p99: 4.2 ms
```

## Profiling Timestamps

### Source Timestamps

**`source_ns`**: When the source generates or receives data internally
- Mock source: When synthetic data is generated
- Application source: When application calls send method
- External sources: When change is detected

**`source_receive_ns`**: When external event is received (HTTP/gRPC sources only)
- HTTP source: When POST request arrives
- gRPC source: When stream message arrives
- Not set for internal sources

**`source_send_ns`**: When source sends event to query
- Set immediately before placing event in channel
- Marks the end of source processing

### Query Timestamps

**`query_receive_ns`**: When query receives event from source
- Set immediately after receiving from channel
- Marks the start of query processing

**`query_core_call_ns`**: Before calling into drasi-core query engine
- Set immediately before continuous query evaluation
- Useful for measuring query overhead vs core processing

**`query_core_return_ns`**: After returning from drasi-core query engine
- Set immediately after continuous query evaluation
- Core processing time = `query_core_return_ns - query_core_call_ns`

**`query_send_ns`**: When query sends result to reaction
- Set immediately before placing result in channel
- Marks the end of query processing

### Reaction Timestamps

**`reaction_receive_ns`**: When reaction receives result from query
- Set immediately after receiving from channel
- Marks the start of reaction processing

**`reaction_complete_ns`**: When reaction finishes processing
- HTTP reaction: After HTTP request completes
- gRPC reaction: After gRPC call completes
- Log reaction: After logging completes
- Application reaction: After processing callback returns

## ProfilerReaction

ProfilerReaction automatically collects and analyzes profiling data from query results.

### Configuration

```rust
Reaction::profiler("profiler")
    .subscribe_to("my-query")
    .with_properties(
        Properties::new()
            .with_int("window_size", 1000)        // Keep last N samples
            .with_int("log_interval_seconds", 30) // Log frequency
    )
    .build()
```

**Configuration Options:**

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `window_size` | int | 1000 | Number of recent samples to keep for percentile calculation |
| `log_interval_seconds` | int | 30 | How often to log statistics |

### Calculated Metrics

ProfilerReaction calculates 5 latency metrics:

1. **Source → Query Latency**: `query_receive_ns - source_send_ns`
   - Measures channel latency and backpressure between source and query

2. **Query → Reaction Latency**: `reaction_receive_ns - query_send_ns`
   - Measures channel latency and backpressure between query and reaction

3. **Query Core Processing Time**: `query_core_return_ns - query_core_call_ns`
   - Measures time spent in continuous query evaluation
   - This is where your Cypher query performance matters

4. **Reaction Processing Time**: `reaction_complete_ns - reaction_receive_ns`
   - Measures time spent in reaction processing
   - HTTP calls, database writes, etc.

5. **Total End-to-End Latency**: `reaction_complete_ns - source_send_ns`
   - Complete latency from source to reaction

### Statistics Reported

For each metric, ProfilerReaction reports:

- **Count**: Total number of samples
- **Mean**: Average latency
- **Standard Deviation**: Measure of variability
- **Min**: Minimum observed latency
- **Max**: Maximum observed latency
- **p50 (Median)**: 50th percentile
- **p95**: 95th percentile (important for SLOs)
- **p99**: 99th percentile (captures tail latency)

### Statistical Methods

**Welford's Algorithm**: For efficient online calculation of mean and variance
- No need to store all samples
- Numerically stable
- O(1) memory, O(1) update time

**Sliding Window**: For percentile calculation
- VecDeque maintains last `window_size` samples
- Sorted on-demand for percentile queries
- Trade-off: memory for accuracy

## Manual Profiling

For custom profiling scenarios, use the profiling API directly:

### Creating Profiling Metadata

```rust
use drasi_server_core::profiling::{ProfilingMetadata, timestamp_ns};

let mut profiling = ProfilingMetadata::new();
profiling.source_send_ns = Some(timestamp_ns());
```

### Passing Through Source

```rust
use drasi_server_core::channels::{SourceEvent, SourceEventWrapper};
use drasi_core::models::SourceChange;

let event = SourceEventWrapper::with_profiling(
    "my-source".to_string(),
    SourceEvent::Change(change),
    chrono::Utc::now(),
    profiling,
);

// Send event through data router...
```

### Accessing in Reactions

```rust
// In a custom reaction implementation
async fn process_result(&self, query_result: QueryResult) {
    if let Some(profiling) = &query_result.profiling {
        // Calculate custom latencies
        if let (Some(source), Some(reaction)) =
            (profiling.source_send_ns, profiling.reaction_complete_ns) {
            let total_latency_ns = reaction - source;
            let total_latency_ms = total_latency_ns as f64 / 1_000_000.0;

            log::info!("Total latency: {:.3} ms", total_latency_ms);
        }
    }
}
```

### Application Source Profiling

Application sources can manually control profiling:

```rust
use drasi_server_core::profiling::{ProfilingMetadata, timestamp_ns};
use drasi_server_core::sources::application::PropertyMapBuilder;

let source = core.source_handle("app-source")?;

// Create profiling metadata
let mut profiling = ProfilingMetadata::new();
profiling.source_ns = Some(timestamp_ns());

// Send with profiling
source.send_node_insert_with_profiling(
    "node-1",
    vec!["Node"],
    PropertyMapBuilder::new()
        .with_string("key", "value")
        .build(),
    profiling
).await?;
```

## Source Configuration

Most sources support profiling via the `enable_profiling` property:

### Mock Source

```rust
Source::mock("test")
    .with_properties(
        Properties::new()
            .with_bool("enable_profiling", true)
    )
    .build()
```

```yaml
sources:
  - id: test
    source_type: mock
    properties:
      enable_profiling: true
```

### PostgreSQL Source

```rust
Source::postgres("db")
    .with_properties(
        Properties::new()
            .with_bool("enable_profiling", true)
            .with_string("host", "localhost")
            // ... other properties
    )
    .build()
```

### HTTP Source

```rust
Source::http("webhook")
    .with_properties(
        Properties::new()
            .with_bool("enable_profiling", true)
            .with_int("port", 9000)
    )
    .build()
```

HTTP source sets `source_receive_ns` when request arrives, then `source_send_ns` when forwarding.

### gRPC Source

```rust
Source::grpc("stream")
    .with_properties(
        Properties::new()
            .with_bool("enable_profiling", true)
            .with_int("port", 50051)
    )
    .build()
```

gRPC source sets `source_receive_ns` when stream message arrives.

### Platform Source

```rust
Source::platform("redis-stream")
    .with_properties(
        Properties::new()
            .with_bool("enable_profiling", true)
            .with_string("redis_url", "redis://localhost")
            .with_string("stream_key", "changes")
    )
    .build()
```

### Application Source

Application sources always support profiling via the API:

```rust
// Without profiling (uses None internally)
source.send_node_insert(...).await?;

// With profiling
let profiling = ProfilingMetadata::new();
source.send_node_insert_with_profiling(..., profiling).await?;
```

## Statistical Methods

### Welford's Algorithm

ProfilerReaction uses Welford's algorithm for online variance calculation:

```rust
// Update mean and variance incrementally
count += 1;
delta = value - mean;
mean += delta / count;
m2 += delta * (value - mean);

// Variance = m2 / (count - 1)
// Std Dev = sqrt(variance)
```

**Benefits:**
- No need to store all samples
- Numerically stable (avoids catastrophic cancellation)
- Constant memory usage
- Single-pass algorithm

### Percentile Calculation

ProfilerReaction maintains a sliding window of recent samples:

```rust
// Store recent samples in VecDeque
samples.push_back(profiling);
if samples.len() > window_size {
    samples.pop_front();
}

// Calculate percentiles by sorting
let mut sorted: Vec<_> = samples.iter()
    .filter_map(|p| calculate_latency(p))
    .collect();
sorted.sort_unstable();

// p50 = sorted[len / 2]
// p95 = sorted[len * 95 / 100]
// p99 = sorted[len * 99 / 100]
```

**Trade-offs:**
- Memory: O(window_size) samples stored
- CPU: O(N log N) for sorting (done once per log interval)
- Accuracy: Exact percentiles (not approximations)

### Timestamp Precision

Timestamps use `std::time::Instant` which provides:
- Nanosecond resolution on most platforms
- Monotonic (never goes backwards)
- High precision (typically RDTSC on x86)

```rust
pub fn timestamp_ns() -> u64 {
    std::time::Instant::now()
        .elapsed()
        .as_nanos() as u64
}
```

## Performance Impact

### Overhead

Profiling adds minimal overhead:

| Operation | Overhead |
|-----------|----------|
| Timestamp capture | ~20-50 ns (RDTSC instruction) |
| Metadata allocation | Amortized via Arc cloning |
| Channel transfer | Zero-copy (metadata is optional) |
| **Total per event** | **~100 ns** |

### Memory Impact

| Component | Memory Usage |
|-----------|--------------|
| ProfilingMetadata | 72 bytes (9 × Option<u64>) |
| ProfilerReaction window | 72 KB for 1000 samples |
| Welford's state | ~40 bytes per metric |
| **Total** | **~72 KB + overhead** |

### When to Enable

✅ **Enable profiling when:**
- Investigating performance issues
- Establishing performance baselines
- Monitoring production SLOs (sample %, not all events)
- Running performance benchmarks

❌ **Disable profiling when:**
- Processing extremely high volumes (> 100K events/sec)
- Memory constrained environments
- Profiling overhead matters (ultra-low latency requirements)

### Sampling Strategy

For high-volume scenarios, sample profiling:

```rust
let mut counter = 0;

// In your source implementation
counter += 1;
let enable_profiling = counter % 100 == 0;  // Profile 1% of events

if enable_profiling {
    profiling = Some(ProfilingMetadata::new());
    profiling.as_mut().unwrap().source_send_ns = Some(timestamp_ns());
}
```

## Best Practices

### 1. Choose Appropriate Window Size

**Small windows (100-500)**: Show recent trends, faster percentile calculation
- Use for short-term monitoring
- Useful during active debugging
- Quick to respond to changes

**Large windows (1000-10000)**: Stable statistics, smooth out spikes
- Use for long-term monitoring
- Better for SLO tracking
- More representative of overall behavior

### 2. Monitor p99, Not Just Mean

The 99th percentile reveals tail latency that affects user experience:

```
Mean: 10 ms  ← Looks great!
p99:  2 s    ← 1% of requests take 2 seconds!
```

### 3. Identify Bottlenecks by Comparing Metrics

| High Metric | Probable Cause | Solution |
|-------------|----------------|----------|
| Source → Query | Source slow, backpressure | Optimize source, increase buffer |
| Query Core Time | Complex query, large graph | Optimize Cypher, add indexes |
| Reaction Time | Slow HTTP calls, DB writes | Optimize reaction, batch, async |
| Query → Reaction | Reaction backpressure | Scale reactions, increase throughput |

### 4. Set Realistic SLOs

Use profiling data to set data-driven SLOs:

```
Example SLO:
- p50 latency < 5 ms
- p95 latency < 20 ms
- p99 latency < 100 ms
```

### 5. Correlate with System Metrics

Combine profiling with system metrics:
- CPU usage
- Memory usage
- Network latency
- Disk I/O

Example: High query core time + high CPU → query is CPU-bound

### 6. Use Profiling in CI/CD

Add profiling tests to catch regressions:

```rust
#[tokio::test]
async fn test_query_latency_regression() {
    let core = create_test_server_with_profiling().await?;

    // Process 1000 events
    for i in 0..1000 {
        source.send_node_insert(...).await?;
    }

    // Check p95 latency
    let stats = get_profiling_stats(&core).await?;
    assert!(stats.query_core_p95_ms < 10.0, "Query latency regression!");
}
```

## Troubleshooting

### Profiling Data Not Appearing

**Problem**: ProfilerReaction not logging statistics

**Checklist:**
1. ✓ Source has `enable_profiling: true`?
2. ✓ ProfilerReaction is subscribed to correct query?
3. ✓ Events are flowing through the pipeline?
4. ✓ `log_interval_seconds` has passed?

**Solution:**
```rust
// Check if events have profiling
if let Some(ref profiling) = query_result.profiling {
    log::info!("Profiling data present: {:?}", profiling);
} else {
    log::warn!("No profiling data in query result!");
}
```

### Incomplete Timestamp Data

**Problem**: Some timestamps are None

**Cause**: Timestamps are optional and may not be set in all scenarios

**Solution**: Check which timestamps are missing to identify where profiling stops:

```rust
if let Some(profiling) = &query_result.profiling {
    if profiling.query_core_call_ns.is_none() {
        log::warn!("Query core timestamps missing - check QueryManager");
    }
}
```

### High Memory Usage

**Problem**: ProfilerReaction using too much memory

**Cause**: Large `window_size` setting

**Solution**: Reduce window size or enable sampling:

```rust
// Reduce window
Properties::new().with_int("window_size", 100)

// Or sample events in source (profile every Nth event)
```

### Timestamps Out of Order

**Problem**: Timestamps don't increase monotonically

**Cause 1**: Clock skew (if using system time instead of Instant)
- **Solution**: Use `timestamp_ns()` which uses monotonic `Instant`

**Cause 2**: Concurrent processing reordering events
- **Solution**: This is expected; use statistics to understand distribution

### Percentiles Look Wrong

**Problem**: p99 seems too low/high

**Cause**: Window size doesn't have enough samples

**Solution**: Ensure window_size is large enough:
- For p99, need at least 100 samples
- For p95, need at least 20 samples
- Recommended: window_size ≥ 1000

## Examples

### Basic Profiling Usage

See `examples/profiling_basic.rs`:

```bash
cargo run --example profiling_basic
```

This example shows:
- Creating profiling metadata manually
- Simulating timestamps at each stage
- Calculating latencies
- Understanding the data flow

### ProfilerReaction with Statistics

See `examples/profiling_with_profiler_reaction.rs`:

```bash
cargo run --example profiling_with_profiler_reaction
```

This example demonstrates:
- Configuring ProfilerReaction
- Generating events with varying latencies
- Automatic statistics collection
- Interpreting profiling output

### Integration Test

See `tests/profiling_integration_test.rs`:

```bash
cargo test profiling
```

This test suite covers:
- End-to-end profiling flow
- Timestamp ordering verification
- Channel preservation of profiling data
- Serialization/deserialization
- Optional timestamp handling

## API Reference

### Types

```rust
// Core profiling structure
pub struct ProfilingMetadata {
    pub source_ns: Option<u64>,
    pub source_receive_ns: Option<u64>,
    pub source_send_ns: Option<u64>,
    pub query_receive_ns: Option<u64>,
    pub query_core_call_ns: Option<u64>,
    pub query_core_return_ns: Option<u64>,
    pub query_send_ns: Option<u64>,
    pub reaction_receive_ns: Option<u64>,
    pub reaction_complete_ns: Option<u64>,
}

impl ProfilingMetadata {
    pub fn new() -> Self;
}
```

### Functions

```rust
// Get current timestamp in nanoseconds
pub fn timestamp_ns() -> u64;
```

### Helper Constructors

```rust
// SourceEventWrapper with profiling
pub fn with_profiling(
    source_id: String,
    event: SourceEvent,
    timestamp: DateTime<Utc>,
    profiling: ProfilingMetadata,
) -> Self;

// QueryResult with profiling
pub fn with_profiling(
    query_id: String,
    timestamp: DateTime<Utc>,
    results: Vec<Value>,
    metadata: HashMap<String, Value>,
    profiling: ProfilingMetadata,
) -> Self;
```

## Conclusion

DrasiServerCore's profiling system provides comprehensive visibility into end-to-end latency with minimal overhead. By capturing timestamps at 9 key points and automatically calculating statistics, you can:

- Identify performance bottlenecks
- Monitor SLA compliance
- Optimize resource usage
- Make data-driven scaling decisions

For questions or issues, please visit the [GitHub repository](https://github.com/drasi-project/drasi-core).
