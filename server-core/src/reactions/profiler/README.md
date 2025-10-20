# Profiler Reaction

## Purpose

The Profiler reaction is a specialized reaction in DrasiServerCore that automatically collects, analyzes, and reports performance statistics for query result processing. It provides detailed insights into latency at every stage of the data pipeline from source to reaction completion.

### What It Does

The Profiler reaction:
- Collects profiling metadata from query results as they flow through the system
- Calculates real-time statistics using Welford's algorithm for online variance computation
- Maintains a sliding window of recent samples for accurate percentile calculation
- Periodically logs comprehensive performance reports with mean, standard deviation, min, max, and percentiles (p50, p95, p99)
- Tracks five key latency metrics:
  - Source to Query latency (channel latency between source and query)
  - Query Processing time (time spent in drasi-core query evaluation)
  - Query to Reaction latency (channel latency between query and reaction)
  - Reaction Processing time (time spent in reaction processing)
  - Total End-to-End latency (complete pipeline latency)

### Use Cases

**Performance Monitoring and Optimization:**
- Continuously monitor pipeline performance in development and production environments
- Identify performance bottlenecks by comparing latencies across different pipeline stages
- Track performance over time to detect degradation or improvements

**SLA/SLO Compliance Tracking:**
- Monitor p95 and p99 percentiles for Service Level Agreement compliance
- Set alerts based on tail latency metrics
- Validate that your continuous query system meets performance requirements

**Capacity Planning:**
- Understand latency distributions to plan for scaling
- Identify when components are becoming saturated (increasing p99)
- Make data-driven decisions about resource allocation

**Debugging and Troubleshooting:**
- Quickly identify which stage of the pipeline is causing delays
- Analyze latency patterns during specific time windows
- Correlate performance issues with system events

### When to Use This Reaction

Use the Profiler reaction when:
- You need visibility into end-to-end query processing performance
- You want to establish performance baselines for your continuous queries
- You're investigating latency issues or performance degradation
- You need statistical data for capacity planning decisions
- You want to monitor SLA compliance for query processing

### How It Complements Other Monitoring Tools

The Profiler reaction is designed specifically for DrasiServerCore pipeline performance analysis and complements other monitoring approaches:

**System Metrics (CPU, Memory, Network):**
- System metrics show resource utilization
- Profiler shows how that utilization translates to application-level latency
- Correlation: High query processing time + high CPU → query is CPU-bound

**Application Logs:**
- Logs provide event-level details
- Profiler provides aggregated statistical view
- Use logs for specific event investigation, profiler for trends

**APM Tools (Application Performance Monitoring):**
- APM tools provide distributed tracing across services
- Profiler provides detailed in-process pipeline metrics
- Use both for complete visibility: APM for inter-service latency, Profiler for intra-process stages

**Custom Metrics/Prometheus:**
- Custom metrics can export specific counters/gauges
- Profiler provides comprehensive latency statistics without custom instrumentation
- Consider exporting profiler statistics to metrics systems for alerting (future enhancement)

## Configuration Properties

The Profiler reaction accepts the following configuration properties:

### `window_size`

- **Type:** Integer (positive)
- **Default:** 1000
- **Description:** Number of recent profiling samples to retain for percentile calculation. The profiler maintains a sliding window of this size to compute accurate percentiles (p50, p95, p99). Older samples are discarded as new samples arrive. Larger windows provide more stable percentiles but consume more memory.
- **Required:** No
- **Memory Impact:** ~72 bytes per sample (window_size × 72 bytes)
- **Recommendations:**
  - Small windows (100-500): Show recent trends, respond quickly to changes
  - Medium windows (1000-5000): Balance stability and responsiveness (recommended)
  - Large windows (5000-10000): Very stable statistics, good for long-term monitoring

### `report_interval_secs`

- **Type:** Integer (positive)
- **Default:** 10
- **Description:** How often (in seconds) the profiler logs statistical reports. The profiler accumulates profiling data continuously and generates reports at this interval. Reports include all statistics for the five key latency metrics.
- **Required:** No
- **Recommendations:**
  - Short intervals (5-15s): Good for active debugging and development
  - Medium intervals (30-60s): Suitable for production monitoring
  - Long intervals (120-300s): For low-overhead long-term monitoring

## Configuration Examples

### YAML Configuration

#### Basic Configuration

```yaml
reactions:
  - id: pipeline-profiler
    reaction_type: profiler
    queries:
      - my-query
    auto_start: true
    properties:
      window_size: 1000
      report_interval_secs: 30
```

#### Development Configuration (Frequent Reports)

```yaml
reactions:
  - id: dev-profiler
    reaction_type: profiler
    queries:
      - user-query
      - order-query
    auto_start: true
    properties:
      window_size: 500
      report_interval_secs: 10
```

#### Production Configuration (Larger Window, Less Frequent)

```yaml
reactions:
  - id: prod-profiler
    reaction_type: profiler
    queries:
      - critical-query
    auto_start: true
    properties:
      window_size: 5000
      report_interval_secs: 60
```

#### Minimal Configuration (Uses Defaults)

```yaml
reactions:
  - id: simple-profiler
    reaction_type: profiler
    queries:
      - my-query
    auto_start: true
    # Uses default window_size: 1000 and report_interval_secs: 10
```

### JSON Configuration

#### Basic Configuration

```json
{
  "reactions": [
    {
      "id": "pipeline-profiler",
      "reaction_type": "profiler",
      "queries": ["my-query"],
      "auto_start": true,
      "properties": {
        "window_size": 1000,
        "report_interval_secs": 30
      }
    }
  ]
}
```

#### Multiple Queries with Different Settings

```json
{
  "reactions": [
    {
      "id": "fast-query-profiler",
      "reaction_type": "profiler",
      "queries": ["fast-query"],
      "auto_start": true,
      "properties": {
        "window_size": 500,
        "report_interval_secs": 10
      }
    },
    {
      "id": "slow-query-profiler",
      "reaction_type": "profiler",
      "queries": ["slow-query"],
      "auto_start": true,
      "properties": {
        "window_size": 2000,
        "report_interval_secs": 60
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Creating a Profiler Reaction Configuration

```rust
use drasi_server_core::config::ReactionConfig;
use std::collections::HashMap;

// Create properties map
let mut properties = HashMap::new();
properties.insert("window_size".to_string(), serde_json::json!(1000));
properties.insert("report_interval_secs".to_string(), serde_json::json!(30));

// Create reaction configuration
let config = ReactionConfig {
    id: "my-profiler".to_string(),
    reaction_type: "profiler".to_string(),
    queries: vec!["my-query".to_string()],
    auto_start: true,
    properties,
};
```

### Creating and Starting the Profiler Reaction

```rust
use drasi_server_core::{
    config::ReactionConfig,
    reactions::{ProfilerReaction, Reaction},
};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create properties
    let mut properties = std::collections::HashMap::new();
    properties.insert("window_size".to_string(), serde_json::json!(2000));
    properties.insert("report_interval_secs".to_string(), serde_json::json!(30));

    // Create configuration
    let config = ReactionConfig {
        id: "profiler".to_string(),
        reaction_type: "profiler".to_string(),
        queries: vec!["example-query".to_string()],
        auto_start: true,
        properties,
    };

    // Create channels
    let (event_tx, event_rx) = mpsc::channel(100);
    let (result_tx, result_rx) = mpsc::channel(100);

    // Create profiler reaction
    let profiler = ProfilerReaction::new(config, event_tx);

    // Start the profiler (it will begin collecting data and logging reports)
    tokio::spawn(async move {
        if let Err(e) = profiler.start(result_rx).await {
            eprintln!("Profiler error: {}", e);
        }
    });

    // Your query results will flow through result_tx
    // and the profiler will automatically collect statistics

    Ok(())
}
```

### Setting Custom Profiling Parameters

```rust
use drasi_server_core::{
    config::ReactionConfig,
    reactions::{ProfilerReaction, Reaction},
};

fn create_profiler_with_custom_params(
    window_size: usize,
    report_interval_secs: u64,
    query_ids: Vec<String>,
) -> ProfilerReaction {
    let mut properties = std::collections::HashMap::new();
    properties.insert("window_size".to_string(), serde_json::json!(window_size));
    properties.insert(
        "report_interval_secs".to_string(),
        serde_json::json!(report_interval_secs),
    );

    let config = ReactionConfig {
        id: "custom-profiler".to_string(),
        reaction_type: "profiler".to_string(),
        queries: query_ids,
        auto_start: true,
        properties,
    };

    let (event_tx, _) = tokio::sync::mpsc::channel(100);
    ProfilerReaction::new(config, event_tx)
}

// Usage
let profiler = create_profiler_with_custom_params(
    5000,  // Large window for stable statistics
    60,    // Report every minute
    vec!["query1".to_string(), "query2".to_string()],
);
```

## Input Data Format

### QueryResult Structure

The Profiler reaction receives `QueryResult` objects from queries. For profiling to work, these results must include profiling metadata:

```rust
pub struct QueryResult {
    pub query_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<serde_json::Value>,
    pub metadata: HashMap<String, serde_json::Value>,

    // Profiling metadata (optional)
    pub profiling: Option<ProfilingMetadata>,
}
```

### ProfilingMetadata Structure

The profiling metadata contains timestamps captured at each stage of processing:

```rust
pub struct ProfilingMetadata {
    // Source timestamps
    pub source_ns: Option<u64>,              // External source timestamp (if available)
    pub source_receive_ns: Option<u64>,      // When source received the event
    pub source_send_ns: Option<u64>,         // When source sent to query

    // Query timestamps
    pub query_receive_ns: Option<u64>,       // When query received from source
    pub query_core_call_ns: Option<u64>,     // Before drasi-core evaluation
    pub query_core_return_ns: Option<u64>,   // After drasi-core evaluation
    pub query_send_ns: Option<u64>,          // When query sent to reaction

    // Reaction timestamps
    pub reaction_receive_ns: Option<u64>,    // When reaction received from query
    pub reaction_complete_ns: Option<u64>,   // When reaction finished processing
}
```

All timestamps are in nanoseconds since UNIX epoch (obtained via `std::time::SystemTime`).

### Ensuring Profiling Data Is Included

To ensure profiling data is included in query results:

#### 1. Enable Profiling in Source Configuration

Most sources support the `enable_profiling` property:

```yaml
sources:
  - id: my-source
    source_type: postgres  # or http, grpc, mock, platform, etc.
    properties:
      enable_profiling: true
      # ... other source properties
```

```rust
use drasi_server_core::sources::SourceConfig;

let mut properties = std::collections::HashMap::new();
properties.insert("enable_profiling".to_string(), serde_json::json!(true));

let source_config = SourceConfig {
    id: "my-source".to_string(),
    source_type: "postgres".to_string(),
    properties,
    // ...
};
```

#### 2. Profiling in Application Sources

For application sources, use the profiling-aware API:

```rust
use drasi_server_core::profiling::{ProfilingMetadata, timestamp_ns};

// Create profiling metadata
let mut profiling = ProfilingMetadata::new();
profiling.source_ns = Some(timestamp_ns());

// Send event with profiling
source_handle.send_node_insert_with_profiling(
    "node-1",
    vec!["Label"],
    properties,
    profiling,
).await?;
```

#### 3. Verify Profiling Data Flow

The DrasiServerCore query manager automatically propagates profiling metadata through the pipeline:
- Source sets initial timestamps → `source_send_ns`
- Query adds its timestamps → `query_receive_ns`, `query_core_call_ns`, etc.
- Reaction receives complete metadata → `reaction_receive_ns`

If profiling data is missing, check that:
- The source has `enable_profiling: true` set
- Events are flowing through the normal pipeline (not bypassing channels)
- The query is configured to receive from that source

## Output Data Format

### Profiling Report Structure

The Profiler reaction logs reports to stdout/stderr using standard Rust logging (via the `log` crate). Reports are generated at the configured `report_interval_secs` and include comprehensive statistics for all five latency metrics.

### Metrics Reported

#### 1. Source→Query Latency
**Calculation:** `query_receive_ns - source_send_ns`

**What it measures:** Channel latency and potential backpressure between the source and query components. High values indicate the query is slow to consume source events.

#### 2. Query Processing Time
**Calculation:** `query_core_return_ns - query_core_call_ns`

**What it measures:** Time spent in drasi-core continuous query evaluation. This is where your Cypher query logic executes. High values indicate expensive query operations.

#### 3. Query→Reaction Latency
**Calculation:** `reaction_receive_ns - query_send_ns`

**What it measures:** Channel latency and potential backpressure between the query and reaction components. High values indicate the reaction is slow to consume query results.

#### 4. Reaction Processing Time
**Calculation:** `reaction_complete_ns - reaction_receive_ns`

**What it measures:** Time spent processing the result in the reaction (HTTP calls, database writes, logging, etc.). High values indicate expensive reaction operations.

#### 5. Total End-to-End Latency
**Calculation:** `reaction_complete_ns - source_send_ns`

**What it measures:** Complete pipeline latency from when the source sends an event to when the reaction completes processing. This is the most important metric for end-user experience.

### Statistics for Each Metric

For each of the five metrics above, the profiler reports:

- **count:** Total number of samples collected since startup
- **mean:** Average latency in milliseconds
- **stddev (standard deviation):** Measure of variability/dispersion in milliseconds
- **min:** Minimum observed latency in milliseconds
- **p50 (median):** 50th percentile - half of samples are faster, half slower
- **p95:** 95th percentile - 95% of samples are faster than this value
- **p99:** 99th percentile - 99% of samples are faster than this value (tail latency)
- **max:** Maximum observed latency in milliseconds

### Example Log Output

```
INFO [profiler] ========== Profiling Report ==========
INFO [profiler] Source→Query: mean=0.24ms, stddev=0.08ms, min=0.12ms, p50=0.23ms, p95=0.39ms, p99=0.52ms, max=0.89ms (n=1000)
INFO [profiler] Query Processing: mean=1.15ms, stddev=0.35ms, min=0.45ms, p50=1.10ms, p95=1.85ms, p99=2.30ms, max=3.75ms (n=1000)
INFO [profiler] Query→Reaction: mean=0.16ms, stddev=0.04ms, min=0.10ms, p50=0.15ms, p95=0.23ms, p99=0.31ms, max=0.45ms (n=1000)
INFO [profiler] Reaction Processing: mean=0.85ms, stddev=0.21ms, min=0.40ms, p50=0.82ms, p95=1.25ms, p99=1.65ms, max=2.10ms (n=1000)
INFO [profiler] Total End-to-End: mean=2.48ms, stddev=0.62ms, min=1.18ms, p50=2.41ms, p95=3.58ms, p99=4.18ms, max=5.28ms (n=1000)
INFO [profiler] ======================================
```

### Understanding the Output

Each line shows statistics for one metric in the format:
```
<Metric Name>: mean=<avg>ms, stddev=<sd>ms, min=<min>ms, p50=<median>ms, p95=<p95>ms, p99=<p99>ms, max=<max>ms (n=<count>)
```

- All values are in milliseconds (ms) for human readability
- Internal calculations use nanoseconds for precision
- The `(n=<count>)` shows the total number of samples collected

### When No Data Is Available

If no profiling data has been collected yet, the profiler logs:
```
INFO [profiler] No profiling data collected yet
```

This can occur if:
- No query results have been received yet
- Query results do not contain profiling metadata
- The source does not have profiling enabled

## Statistical Methods

### Welford's Algorithm for Online Variance Calculation

The Profiler reaction uses Welford's algorithm for computing mean and variance in a single pass without storing all samples. This provides:

**Benefits:**
- **Constant Memory:** O(1) space regardless of sample count
- **Numerical Stability:** Avoids catastrophic cancellation that occurs in naive variance calculations
- **Online Updates:** New samples are incorporated immediately without reprocessing
- **Efficiency:** O(1) time per sample update

**Algorithm:**
```rust
// For each new sample value:
count += 1
delta = value - mean
mean += delta / count
delta2 = value - mean
m2 += delta * delta2

// Variance is computed as:
variance = m2 / (count - 1)
std_dev = sqrt(variance)
```

**Implementation Details:**
- Maintains running state for each of the five metrics
- Uses f64 (64-bit floating point) for numerical precision
- Calculates sample variance (divides by n-1, not n) for unbiased estimation
- Only one variance value is undefined (returns 0.0) when count < 2

### Percentile Calculation Methodology

The Profiler reaction computes exact percentiles by storing recent samples in a sliding window:

**Process:**
1. Store the most recent `window_size` samples in a VecDeque (double-ended queue)
2. When generating a report, extract values for the metric being calculated
3. Sort the values in ascending order
4. Calculate percentile indices using linear interpolation
5. Return the value at each percentile index

**Formula:**
```rust
// For percentile p (0.50 for p50, 0.95 for p95, etc.):
index = round(p * (len - 1))
percentile_value = sorted_values[index]
```

**Implementation:**
```rust
fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    let index = (p * (sorted_values.len() - 1) as f64).round() as usize;
    sorted_values[index]
}
```

**Complexity:**
- Space: O(window_size) - stores all samples in window
- Time: O(N log N) per report for sorting, where N = window_size
- Sorting happens only at report time, not on every sample

### Sliding Window Behavior

**Window Management:**
- Uses `VecDeque<ProfilingMetadata>` for efficient push/pop operations
- When a new sample arrives:
  - Add to back of queue: `samples.push_back(profiling)`
  - If size exceeds window_size, remove from front: `samples.pop_front()`
- This implements a FIFO (First-In-First-Out) sliding window

**Window Size Impact:**
- Small window (100-500): Shows recent trends, more volatile percentiles
- Medium window (1000-2000): Balanced view, recommended default
- Large window (5000+): Very stable percentiles, less responsive to changes

**Important:** The sliding window only affects percentile calculations. Mean, variance, and standard deviation are calculated using Welford's algorithm over ALL samples since startup, not just the window.

### Statistical Accuracy Considerations

**Percentile Accuracy:**
- Exact percentiles (not approximations like t-digest)
- Requires at least 100 samples for meaningful p99 values
- Requires at least 20 samples for meaningful p95 values
- With small sample counts, percentiles may be less representative

**Mean and Variance:**
- Mean is exact for all samples collected
- Variance uses Bessel's correction (n-1 denominator) for unbiased estimation
- Accuracy is independent of window size
- Welford's algorithm is numerically stable even with billions of samples

**Timestamp Precision:**
- Uses nanosecond timestamps from `std::time::SystemTime`
- SystemTime provides ~100ns resolution on most platforms
- Sufficient precision for measuring sub-millisecond latencies
- Monotonic (never goes backwards)

**Potential Statistical Issues:**
- **Small sample sizes:** Percentiles become unreliable with < 20 samples
- **Extreme outliers:** Can skew mean; use percentiles to understand distribution
- **Non-normal distributions:** Continuous query latencies are often multi-modal (fast path vs slow path), so examine full percentile spectrum

## Interpreting Results

### What Each Metric Means

#### Source→Query Latency
**Indicates:** Channel performance and backpressure between source and query

**Typical Values:**
- Healthy: < 1ms (usually microseconds)
- Warning: 1-10ms
- Critical: > 10ms

**What it tells you:**
- Low values: Query is keeping up with source event rate
- High values: Query is falling behind, events are queuing up
- Rising trend: Query is becoming saturated

#### Query Processing Time
**Indicates:** Continuous query evaluation performance in drasi-core

**Typical Values:**
- Simple queries: < 1ms
- Medium complexity: 1-10ms
- Complex queries: 10-100ms
- Very complex: > 100ms

**What it tells you:**
- This is where your Cypher query executes
- Dominant contributor to end-to-end latency in most cases
- Directly affected by query complexity and data size

#### Query→Reaction Latency
**Indicates:** Channel performance and backpressure between query and reaction

**Typical Values:**
- Healthy: < 1ms (usually microseconds)
- Warning: 1-10ms
- Critical: > 10ms

**What it tells you:**
- Low values: Reaction is keeping up with query output rate
- High values: Reaction is falling behind, results are queuing up
- Rising trend: Reaction is becoming saturated

#### Reaction Processing Time
**Indicates:** Time spent in the reaction (HTTP calls, DB writes, etc.)

**Typical Values:**
- Log/Application reactions: < 1ms
- HTTP reactions (local): 1-10ms
- HTTP reactions (remote): 10-100ms
- Database writes: 5-50ms

**What it tells you:**
- For HTTP reactions, includes network round-trip time
- For database reactions, includes write and commit time
- Often the most variable metric due to external dependencies

#### Total End-to-End Latency
**Indicates:** Complete pipeline performance from source to reaction

**Typical Values:**
- Fast pipelines: < 5ms
- Medium pipelines: 5-50ms
- Slow pipelines: 50-500ms
- Very slow: > 500ms

**What it tells you:**
- Most important metric for user experience
- Sum of all component latencies plus channel overhead
- Use this for SLA/SLO monitoring

### How to Identify Performance Bottlenecks

**Step 1: Compare the Five Metrics**

Look at which metric has the highest mean and p95 values:

| Highest Metric | Bottleneck | Solution |
|----------------|------------|----------|
| Query Processing | Complex query or large dataset | Optimize Cypher query, add indexes, filter earlier |
| Reaction Processing | Slow external calls or I/O | Optimize reaction (batching, async, caching) |
| Source→Query | Query backpressure | Scale query processing, simplify query |
| Query→Reaction | Reaction backpressure | Scale reaction, add buffering, increase concurrency |
| All roughly equal | Well-balanced pipeline | Consider overall throughput optimization |

**Step 2: Examine Percentile Spread**

Compare p50, p95, and p99 to understand latency distribution:

```
Scenario A - Consistent Performance:
  p50: 10ms, p95: 12ms, p99: 14ms
  → Tight distribution, predictable performance

Scenario B - Tail Latency Issues:
  p50: 10ms, p95: 50ms, p99: 200ms
  → Wide distribution, investigate p99 causes
```

**Step 3: Monitor Trends Over Time**

Compare reports across multiple intervals:

```
Time 0:   p95: 10ms
Time 30s: p95: 12ms
Time 60s: p95: 18ms
→ Degrading performance, investigate before it gets worse
```

### Normal vs Abnormal Latency Ranges

**Normal Ranges (Typical Production System):**
- Source→Query: 0.1-1ms
- Query Processing: 1-50ms (depends on query complexity)
- Query→Reaction: 0.1-1ms
- Reaction Processing: 1-100ms (depends on reaction type)
- Total End-to-End: 5-150ms

**Warning Signs:**
- Any channel latency > 10ms → Backpressure building up
- Query processing > 100ms → Query may be too complex
- Reaction processing > 500ms → External calls are very slow
- Total latency > 1000ms → System is struggling

**Context Matters:**
- Latency requirements depend on your use case
- Real-time alerting needs < 100ms end-to-end
- Analytics dashboards might tolerate seconds
- Set your own SLOs based on user requirements

### How to Use Percentiles for SLA Monitoring

**Why Percentiles Matter More Than Averages:**

```
Example System:
  Mean: 10ms    ← Looks great!
  p50:  5ms     ← Most requests are fast
  p95:  15ms    ← Still acceptable
  p99:  500ms   ← 1% of users have terrible experience!
```

**Setting SLA Targets:**

Typical SLA structure:
- **p50 < 10ms:** Most requests are very fast
- **p95 < 50ms:** 95% of users have good experience
- **p99 < 200ms:** Even tail latency is acceptable

**p99 is Critical for User Experience:**
- If p99 = 500ms and you process 1000 requests/sec, that's 10 slow requests per second
- 10 unhappy users per second adds up quickly
- Focus on p99 for SLA compliance, not just mean

**Example SLO Configuration:**

```yaml
# Operational SLO for query processing
slo:
  total_latency:
    p50_target_ms: 20
    p95_target_ms: 100
    p99_target_ms: 500

  query_processing:
    p95_target_ms: 50
    p99_target_ms: 200
```

### Correlation Between Different Metrics

**Understanding Relationships:**

1. **Total ≈ Query Processing + Reaction Processing** (when channels are healthy)
   - If this equation doesn't hold, check channel latencies
   - Large discrepancy indicates queueing/backpressure

2. **Rising Source→Query + Stable Query Processing** = Query input backpressure
   - Query can't keep up with source event rate
   - Solution: Reduce source rate or scale query processing

3. **Stable Query→Reaction + Rising Reaction Processing** = Reaction is slowing down
   - External dependency (database, HTTP endpoint) is degrading
   - Solution: Investigate external services, add caching/batching

4. **All metrics rising proportionally** = System-wide performance degradation
   - Could be resource exhaustion (CPU, memory, network)
   - Check system metrics (top, htop, memory usage)

**Example Analysis:**

```
Report 1:
  Source→Query: 0.2ms
  Query Processing: 5ms
  Query→Reaction: 0.2ms
  Reaction Processing: 10ms
  Total: 15.5ms
  → Normal, reaction is dominant component

Report 2 (After Degradation):
  Source→Query: 15ms      ← Spike!
  Query Processing: 5ms   ← Unchanged
  Query→Reaction: 20ms    ← Spike!
  Reaction Processing: 50ms  ← Increased
  Total: 90ms
  → Reaction is slow (50ms), causing backpressure (15ms, 20ms channel delays)
  → Solution: Fix reaction performance first, channels will recover
```

## Profiling Infrastructure

### Integration with Overall Profiling System

The Profiler reaction is one component of DrasiServerCore's comprehensive profiling infrastructure. For complete information about the profiling system, see:

**[/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/docs/PROFILING.md](../../../docs/PROFILING.md)**

That document covers:
- Overall profiling architecture
- Timestamp capture points (9 stages)
- ProfilingMetadata structure and flow
- Source configuration for profiling
- Manual profiling in custom components
- Performance impact of profiling
- Best practices and troubleshooting

### How This Reaction Fits Into the Profiling System

**Profiling Flow:**
```
1. Source: Enables profiling → Sets source timestamps
   ↓ (ProfilingMetadata flows in SourceEventWrapper)

2. Query: Adds query timestamps
   ↓ (ProfilingMetadata flows in QueryResult)

3. Profiler Reaction: Collects, analyzes, reports statistics
   (This component you're reading about)

4. Other Reactions: Can also access profiling data
   (Optional: Custom reactions can read profiling metadata)
```

**Key Points:**
- Profiling metadata flows automatically through the pipeline
- The Profiler reaction is passive - it doesn't modify the data flow
- Multiple reactions can receive the same query results with profiling
- Profiling can be enabled/disabled per source independently

### How to Enable Profiling in Sources and Queries

#### Source Configuration

Most sources support the `enable_profiling` property:

```yaml
sources:
  - id: my-source
    source_type: postgres
    properties:
      enable_profiling: true
      # ... other properties
```

**Supported Sources:**
- PostgreSQL: `enable_profiling: true`
- HTTP: `enable_profiling: true`
- gRPC: `enable_profiling: true`
- Mock: `enable_profiling: true`
- Platform (Redis): `enable_profiling: true`
- Application: Always supported via API (manual profiling)

#### Query Configuration

Queries automatically propagate profiling metadata - no special configuration needed:

```yaml
queries:
  - id: my-query
    query_type: cypher
    query: "MATCH (n:Node) RETURN n"
    sources:
      - my-source  # If my-source has profiling enabled, it flows through
```

The query manager automatically:
- Receives profiling metadata from sources
- Adds query-stage timestamps
- Passes complete metadata to reactions

#### Complete Example

```yaml
sources:
  - id: profiled-source
    source_type: postgres
    properties:
      enable_profiling: true
      host: localhost
      database: mydb

queries:
  - id: my-query
    query_type: cypher
    query: "MATCH (n:User) WHERE n.active = true RETURN n"
    sources:
      - profiled-source

reactions:
  - id: profiler
    reaction_type: profiler
    queries:
      - my-query
    properties:
      window_size: 1000
      report_interval_secs: 30
```

With this configuration, profiling flows automatically from source through query to profiler reaction.

## Troubleshooting

### Missing Profiling Data

**Problem:** Profiler logs "No profiling data collected yet" repeatedly

**Possible Causes:**

1. **Source profiling not enabled**
   - Check: Source configuration has `enable_profiling: true`
   - Solution: Add property to source configuration and restart

2. **No events flowing through pipeline**
   - Check: Query is receiving events from source (check query logs)
   - Solution: Verify source is generating/receiving events

3. **Profiler not subscribed to correct query**
   - Check: Profiler's `queries` list matches actual query IDs
   - Solution: Update profiler configuration with correct query IDs

4. **Query results don't contain profiling metadata**
   - Check: Query result logs show `profiling: Some(...)`
   - Solution: Verify source timestamps are being set

**Debugging Steps:**

```rust
// In a custom reaction or debug code:
async fn debug_profiling(result: &QueryResult) {
    if let Some(ref prof) = result.profiling {
        println!("Profiling present!");
        println!("  source_send_ns: {:?}", prof.source_send_ns);
        println!("  query_receive_ns: {:?}", prof.query_receive_ns);
        println!("  reaction_receive_ns: {:?}", prof.reaction_receive_ns);
    } else {
        println!("No profiling metadata in result!");
    }
}
```

### No Reports Being Generated

**Problem:** Profiler starts but never logs reports

**Possible Causes:**

1. **Report interval not yet elapsed**
   - Check: How long has the profiler been running?
   - Solution: Wait for `report_interval_secs` to pass

2. **No query results received**
   - Check: Query is producing results
   - Solution: Verify query matches data and produces output

3. **Logging level too high**
   - Check: Logging configuration (RUST_LOG environment variable)
   - Solution: Set `RUST_LOG=info` or `RUST_LOG=drasi_server_core=info`

**Verification:**

```bash
# Enable debug logging
export RUST_LOG=info
cargo run

# Or for more detailed logging
export RUST_LOG=drasi_server_core::reactions::profiler=debug
cargo run
```

### Metrics Always Showing Zero

**Problem:** Reports show 0.00ms for all metrics or only some metrics

**Possible Causes:**

1. **Timestamps not being set at expected stages**
   - Check: Which timestamps are None in profiling metadata
   - Solution: Identify which component isn't setting timestamps

2. **Clock issues (timestamps in wrong order)**
   - Check: Verify system time is correct
   - Solution: Synchronize system clock (NTP)

**Debugging Specific Metrics:**

| Metric Shows Zero | Missing Timestamps | Component to Check |
|-------------------|--------------------|--------------------|
| Source→Query | source_send_ns or query_receive_ns | Source or Query manager |
| Query Processing | query_core_call_ns or query_core_return_ns | Query manager (drasi-core integration) |
| Query→Reaction | query_send_ns or reaction_receive_ns | Query manager or Reaction |
| Reaction Processing | reaction_receive_ns or reaction_complete_ns | Reaction implementation |
| Total End-to-End | source_send_ns or reaction_complete_ns | Source or Reaction |

### Window Size Tuning

**Problem:** Percentiles seem unstable or not representative

**Solution Strategies:**

**If percentiles are too volatile (jumping around):**
- Increase window_size to 2000, 5000, or larger
- Larger windows smooth out short-term spikes
- Trade-off: More memory usage, less responsive to changes

**If percentiles are too stale (don't reflect recent changes):**
- Decrease window_size to 500 or 1000
- Smaller windows respond faster to performance changes
- Trade-off: More volatile, requires more samples for stability

**If memory is constrained:**
- Reduce window_size to minimum needed for percentiles
- For p99: minimum 100 samples
- For p95: minimum 20 samples
- Memory usage: window_size × 72 bytes

**Recommended Values by Use Case:**

```yaml
# Active debugging - see recent trends quickly
window_size: 500
report_interval_secs: 5

# Normal production monitoring - balanced
window_size: 1000
report_interval_secs: 30

# Long-term stability monitoring
window_size: 5000
report_interval_secs: 60

# Memory-constrained environment
window_size: 200
report_interval_secs: 30
```

### Report Interval Tuning

**Problem:** Reports are too frequent or too infrequent

**Solution:**

**If reports are too noisy (too frequent):**
- Increase `report_interval_secs` to 30, 60, or higher
- Reduces log volume
- Still captures all events, just reports less often

**If you need faster feedback:**
- Decrease `report_interval_secs` to 5, 10, or 15
- Good for debugging active performance issues
- More log volume but faster iteration

**Best Practices:**
- Development: 5-15 seconds
- Production monitoring: 30-60 seconds
- Long-term archival: 120-300 seconds

### Understanding Statistical Outliers

**Problem:** Max values are much higher than p99, skewing perception

**Explanation:**

This is normal behavior:
```
p99:  50ms  ← 99% of requests
max: 500ms  ← Single outlier

The outlier doesn't significantly affect mean or percentiles
```

**What to do:**

1. **Don't panic about occasional outliers**
   - Systems have natural variability
   - GC pauses, context switches, etc. cause spikes
   - Focus on p99, not max

2. **Investigate if max is consistently high**
   - If every report shows high max, something is wrong
   - If max is occasional, it's probably fine

3. **Look at p99 instead of max for alerting**
   - p99 represents 99% of user experience
   - max represents a single worst case
   - SLAs should be based on p99, not max

**Example Analysis:**

```
Report 1:
  mean: 10ms, p99: 15ms, max: 500ms
  → Outlier present, but p99 is good. Investigate if curious but not urgent.

Report 2:
  mean: 10ms, p99: 480ms, max: 500ms
  → Problem! p99 is close to max, meaning many requests are slow. Investigate urgently.
```

## Limitations

### Statistical Calculation Overhead

**CPU Overhead:**
- Welford's algorithm: O(1) per sample, ~50-100 CPU cycles
- Percentile calculation: O(N log N) per report where N = window_size
- Negligible for window_size < 10,000

**Impact:**
- Normal workloads: < 0.01% CPU overhead
- High event rates (>100K events/sec): ~0.1-1% CPU overhead
- Report generation: Brief CPU spike (microseconds for sorting)

**Mitigation:**
- Increase report_interval_secs to reduce report frequency
- Reduce window_size if percentile precision isn't critical
- Use sampling (future enhancement)

### Memory Usage for Window Storage

**Memory Per Sample:** ~72 bytes (ProfilingMetadata struct)

**Total Memory:**
```
window_size = 1,000   → ~72 KB
window_size = 10,000  → ~720 KB
window_size = 100,000 → ~7.2 MB
```

**Additional Memory:**
- Welford's state: ~40 bytes per metric (negligible)
- Sorting temporary buffer: window_size × 8 bytes during report generation

**Impact:**
- Normal configurations (window_size < 10,000): Negligible
- Very large windows (window_size > 100,000): May impact memory-constrained systems

**Mitigation:**
- Use smaller window_size if memory is limited
- window_size = 500 still provides reasonable percentile estimates
- Monitor memory usage if using window_size > 10,000

### Accuracy with Small Sample Sizes

**Percentile Accuracy Requirements:**
- p50 (median): Needs at least 10 samples
- p95: Needs at least 20 samples
- p99: Needs at least 100 samples

**Impact of Small Samples:**
```
window_size = 50, computing p99:
  → Only 1 sample represents p99 (50 × 0.99 = 49.5 ≈ 49)
  → Not statistically representative
  → p99 will be very volatile
```

**Recommendations:**
- Use window_size ≥ 1000 for stable percentiles
- If window_size < 100, ignore p99 (not meaningful)
- Mean and stddev are accurate with any sample size > 1

**During Startup:**
- First few reports have small sample counts
- Percentiles become more accurate as samples accumulate
- Consider ignoring first 1-2 reports when benchmarking

### Impact of Window Size on Memory

See "Memory Usage for Window Storage" section above.

**Rule of Thumb:**
- 1000 samples ≈ 72 KB (recommended default)
- Each 1000 samples adds ~72 KB
- Safe up to 10,000 samples (720 KB) on most systems

### Not Suitable for Real-Time Alerting

**Why:**
- Reports are periodic (every N seconds), not real-time
- No immediate notification of threshold breaches
- Logs to stdout/stderr only (no push notifications)

**Limitations:**
- Cannot trigger alerts on individual slow events
- Cannot automatically page on-call when p99 > threshold
- Requires parsing logs to extract metrics for alerting systems

**What It IS Good For:**
- Performance monitoring and trending
- Post-hoc analysis of performance issues
- Establishing baseline performance metrics
- Development and debugging

**What It's NOT Good For:**
- Real-time alerting (use metrics + alerting system)
- Automated incident response
- Circuit breaking or rate limiting decisions
- Live SLA enforcement

**Future Enhancement:**
- Export metrics to Prometheus/StatsD
- Support webhooks for threshold breaches
- Integrate with alerting systems

### Log-Only Output (No Metrics Export)

**Current Implementation:**
- Profiler only outputs to logs (via Rust `log` crate)
- No built-in Prometheus, StatsD, or other metrics integration
- No structured metric export (JSON, CSV, etc.)

**Workarounds:**

1. **Parse Logs:**
   ```bash
   # Example: Extract p99 values from logs
   grep "Total End-to-End" logfile.txt | grep -oP "p99=\K[\d.]+"
   ```

2. **Custom Reaction:**
   - Create a custom reaction that receives same query results
   - Extract profiling metadata and export to metrics system
   - Run alongside Profiler reaction

3. **Log Aggregation:**
   - Send logs to centralized system (ELK, Splunk, etc.)
   - Use log parsing to extract metrics
   - Build dashboards from parsed metrics

**Future Enhancement:**
- Built-in Prometheus exporter
- Pluggable output backends (StatsD, InfluxDB, etc.)
- Structured JSON output mode
- Webhook notifications for threshold breaches

## Additional Resources

### Examples

The DrasiServerCore repository includes complete working examples:

**Basic Profiling Example:**
```bash
cargo run --example profiling_basic
```
Location: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/examples/profiling_basic.rs`

**Profiler Reaction Example:**
```bash
cargo run --example profiling_with_profiler_reaction
```
Location: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/examples/profiling_with_profiler_reaction.rs`

### Related Documentation

- **[Overall Profiling Guide](../../../docs/PROFILING.md)** - Comprehensive profiling system documentation
- **[DrasiServerCore README](../../../README.md)** - Main library documentation
- **[Source Documentation](../../sources/README.md)** - Source configuration and profiling support

### Source Code

**Profiler Reaction Implementation:**
`/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/reactions/profiler/mod.rs`

**Profiling Infrastructure:**
`/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/src/profiling/mod.rs`

### Getting Help

For questions, issues, or contributions:
- GitHub Issues: [drasi-project/drasi-core](https://github.com/drasi-project/drasi-core/issues)
- Documentation: [github.com/drasi-project](https://github.com/drasi-project)
